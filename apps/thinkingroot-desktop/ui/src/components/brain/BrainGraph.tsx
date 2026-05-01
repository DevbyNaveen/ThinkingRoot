import { useEffect, useMemo, useRef, useState } from "react";
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";
import { zoom, zoomIdentity, type ZoomBehavior, type ZoomTransform } from "d3-zoom";
import { select } from "d3-selection";
import { motion } from "framer-motion";
import type { BrainEntity, BrainRelation, ClaimRow } from "@/lib/tauri";
import { cn } from "@/lib/utils";

interface Node extends SimulationNodeDatum {
  id: string;
  label: string;
  claim_count: number;
  entity_type: string;
}

interface Link extends SimulationLinkDatum<Node> {
  type: string;
  strength: number;
}

interface Props {
  entities: BrainEntity[];
  relations: BrainRelation[];
  claims?: ClaimRow[];
  searchQuery?: string;
}

function getSemanticColor(type: string): string {
  const t = type.toLowerCase();
  if (t === "definition") return "hsl(280, 70%, 65%)"; // Purple
  if (t === "apisignature") return "hsl(200, 80%, 65%)"; // Blue
  if (t === "architecture") return "hsl(30, 80%, 65%)"; // Orange
  if (t === "rooted") return "hsl(150, 70%, 60%)"; // Emerald
  if (t === "requirement") return "hsl(340, 70%, 65%)"; // Pink
  if (t === "inferred") return "rgba(140, 140, 140, 0.4)"; // Muted silver
  return "rgba(200, 200, 200, 0.8)"; // Default silver
}

// Lower index = higher priority — same ordering as the pre-rewrite
// nested-loop did via `priority.indexOf(...)`, just hoisted into a
// Map so the inner pass is O(1) instead of O(P).
const TYPE_PRIORITY: ReadonlyArray<string> = [
  "definition",
  "apisignature",
  "architecture",
  "requirement",
  "fact",
];
const TYPE_RANK = new Map<string, number>(
  TYPE_PRIORITY.map((t, i) => [t, i] as const),
);

// Escape a string so it can be embedded as a literal inside a regex
// alternation (`|`) without character-class side effects.  Used for
// the H1 entity-name → claim-statement match index.
function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Obsidian-grade canvas graph.
 *
 * Performance posture (post-P6):
 *   - Per-entity "best semantic type" derivation runs a single regex
 *     pass over each claim statement (was O(N×M×P) nested + indexOf).
 *   - Render loop is driven by `simulation.on("tick", ...)` plus
 *     explicit redraws on hover / isolate / search changes.  No
 *     manual `requestAnimationFrame`, so CPU drops to 0 % once the
 *     simulation converges (alpha < alphaMin).
 *   - Hover / isolate / search live in refs so a keystroke in the
 *     parent's search input doesn't tear down the canvas effect and
 *     reinitialise everything on every character.
 */
export function BrainGraph({ entities, relations, claims = [], searchQuery }: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const simulationRef = useRef<ReturnType<typeof forceSimulation<Node>> | null>(
    null,
  );
  const transformRef = useRef<ZoomTransform>(zoomIdentity);
  const zoomBehaviorRef = useRef<ZoomBehavior<HTMLCanvasElement, unknown> | null>(null);

  const [hovered, setHovered] = useState<string | null>(null);
  const [isolated, setIsolated] = useState<string | null>(null);
  const [size, setSize] = useState({ w: 800, h: 600 });

  // Mirrors of UI state read from inside the (stable) draw closure.
  // Updating a ref does NOT retrigger the canvas-init effect — that's
  // exactly the H2 fix.  Pre-rewrite the parent's `searchQuery` prop
  // sat in the render-effect dep list, so every keystroke tore down
  // and rebuilt the canvas pipeline.
  const hoveredRef = useRef<string | null>(null);
  const isolatedRef = useRef<string | null>(null);
  const searchQueryRef = useRef<string | undefined>(undefined);
  const drawRef = useRef<(() => void) | null>(null);

  // 1. Prepare data + adjacency map + per-entity best semantic type.
  const { nodes, links, neighborMap } = useMemo(() => {
    const nameToNode = new Map<string, Node>();
    const neighbors = new Map<string, Set<string>>();
    const bestTypeMap = new Map<string, string>();
    const bestRankMap = new Map<string, number>();

    // P6 / H1: instead of iterating every entity for every claim
    // (the old O(N×M) nested loop), build a single alternation regex
    // from entity names sorted longest-first (so multi-word names
    // win over their prefixes) and run it once per statement.  At
    // 10K claims × 1K entities the old loop did ~10M substring
    // probes per refresh; this version is O(N×L) where L is mean
    // statement length — comfortably under a frame budget.
    const sortedNames = entities
      .map((e) => e.name)
      .filter((n) => n.length > 0)
      .sort((a, b) => b.length - a.length);
    const matcher =
      sortedNames.length > 0
        ? new RegExp(sortedNames.map(escapeRegex).join("|"), "g")
        : null;

    if (matcher) {
      for (const claim of claims) {
        // `claim_type` is optional in the wire schema (the engine
        // omits it for some legacy / structural claims).  Skip the
        // priority update when missing — the entity still gets a
        // type from `entity.entity_type` in the fallback branch
        // below — but the rooted-tier override still applies.
        const incoming = claim.claim_type;
        const incomingRank =
          incoming === undefined
            ? Number.MAX_SAFE_INTEGER
            : (TYPE_RANK.get(incoming.toLowerCase()) ?? Number.MAX_SAFE_INTEGER);
        const isRooted = claim.tier === "rooted";

        // `matchAll` walks every non-overlapping match in one pass.
        const matches = claim.statement.matchAll(matcher);
        for (const m of matches) {
          const name = m[0];
          const currentRank = bestRankMap.get(name);
          if (
            incoming !== undefined &&
            (currentRank === undefined || incomingRank < currentRank)
          ) {
            bestTypeMap.set(name, incoming);
            bestRankMap.set(name, incomingRank);
          }
          // "rooted" tier overrides everything except an explicit
          // `definition` type — same semantic as the pre-rewrite
          // branch, just hoisted out of the inner-most loop.
          if (isRooted) {
            const cur = bestTypeMap.get(name);
            if (!cur || cur.toLowerCase() !== "definition") {
              bestTypeMap.set(name, "rooted");
            }
          }
        }
      }
    }

    for (const e of entities) {
      nameToNode.set(e.name, {
        id: e.name,
        label: e.name,
        claim_count: e.claim_count,
        entity_type: bestTypeMap.get(e.name) || e.entity_type,
      });
    }
    for (const r of relations) {
      for (const name of [r.source, r.target]) {
        if (!nameToNode.has(name)) {
          nameToNode.set(name, {
            id: name,
            label: name,
            claim_count: 0,
            entity_type: bestTypeMap.get(name) || "inferred",
          });
        }
      }
      if (!neighbors.has(r.source)) neighbors.set(r.source, new Set());
      if (!neighbors.has(r.target)) neighbors.set(r.target, new Set());
      neighbors.get(r.source)!.add(r.target);
      neighbors.get(r.target)!.add(r.source);
    }
    const nodeArr = Array.from(nameToNode.values());
    const linkArr: Link[] = relations.map((r) => ({
      source: r.source,
      target: r.target,
      type: r.relation_type,
      strength: r.strength,
    }));
    return { nodes: nodeArr, links: linkArr, neighborMap: neighbors };
  }, [entities, relations, claims]);

  // 2. Initialise physics engine.  Setting `alphaMin` explicitly is
  //    what lets the simulation actually emit an `end` event (default
  //    is 0.001 but we set it for clarity in the pause-on-rest fix).
  useEffect(() => {
    if (nodes.length === 0) return;

    const sim = forceSimulation<Node>(nodes)
      .alphaDecay(0.015)
      .alphaMin(0.001)
      .force(
        "link",
        forceLink<Node, Link>(links).id((d) => d.id).distance(60).strength(0.5),
      )
      .force("charge", forceManyBody().strength(-150))
      .force("center", forceCenter(0, 0))
      .force(
        "collide",
        forceCollide<Node>()
          .radius((d) => 4 + Math.sqrt(d.claim_count) * 1.5)
          .iterations(2),
      );

    simulationRef.current = sim;

    return () => {
      sim.stop();
    };
  }, [nodes, links]);

  // 3. Canvas + render hookup.  Stable deps — only the data shape
  //    or the canvas size can retrigger this effect.  Hover, isolate,
  //    and search are read via refs, so keystrokes do not invalidate
  //    the GPU upload or restart d3-force ticking.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    canvas.width = size.w * dpr;
    canvas.height = size.h * dpr;
    canvas.style.width = `${size.w}px`;
    canvas.style.height = `${size.h}px`;

    const draw = () => {
      ctx.save();
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.scale(dpr, dpr);

      const t = transformRef.current;
      ctx.translate(t.x, t.y);
      ctx.scale(t.k, t.k);

      const activeFocus = hoveredRef.current || isolatedRef.current;
      const activeNeighbors = activeFocus
        ? (neighborMap.get(activeFocus) ?? new Set<string>())
        : new Set<string>();
      const hasFocus = activeFocus !== null;

      const q = searchQueryRef.current?.trim().toLowerCase();
      const hasSearch = !!q;

      // Lines
      links.forEach((l) => {
        const s = l.source as Node;
        const targetNode = l.target as Node;
        if (s.x != null && targetNode.x != null && s.y != null && targetNode.y != null) {
          const isRelated =
            activeFocus && (s.id === activeFocus || targetNode.id === activeFocus);

          ctx.beginPath();
          ctx.moveTo(s.x, s.y);
          ctx.lineTo(targetNode.x, targetNode.y);

          if (isRelated) {
            ctx.strokeStyle = "rgba(100, 200, 255, 0.7)";
            ctx.lineWidth = 1.5 / t.k;
            ctx.stroke();
          } else {
            const opacity = hasFocus || hasSearch ? 0.02 : 0.08;
            ctx.strokeStyle = `rgba(255, 255, 255, ${opacity})`;
            ctx.lineWidth = 0.8 / t.k;
            ctx.stroke();
          }
        }
      });

      // Nodes
      nodes.forEach((n) => {
        if (n.x == null || n.y == null) return;
        const r = Math.max(3, Math.min(12, 3 + Math.sqrt(n.claim_count) * 1.5));
        const isFocused = activeFocus === n.id;
        const isNeighbor = activeNeighbors.has(n.id);
        const isSearchMatch = hasSearch && n.label.toLowerCase().includes(q!);

        if (isFocused || isSearchMatch) {
          ctx.beginPath();
          ctx.arc(n.x, n.y, r + 6 / t.k, 0, 2 * Math.PI);
          ctx.fillStyle =
            isSearchMatch && !isFocused
              ? "rgba(255, 200, 100, 0.25)"
              : "rgba(100, 200, 255, 0.25)";
          ctx.fill();
        }

        ctx.beginPath();
        ctx.arc(n.x, n.y, r, 0, 2 * Math.PI);
        if (isFocused) {
          ctx.fillStyle = "rgb(255, 255, 255)";
        } else if (isNeighbor) {
          ctx.fillStyle = "rgb(100, 200, 255)";
        } else if (isSearchMatch) {
          ctx.fillStyle = "rgb(255, 200, 100)";
        } else {
          const semanticColor = getSemanticColor(n.entity_type);
          ctx.fillStyle =
            hasFocus || hasSearch ? "rgba(100, 100, 100, 0.1)" : semanticColor;
        }
        ctx.fill();

        const showLabel =
          isFocused ||
          isNeighbor ||
          isSearchMatch ||
          (nodes.length < 100 && !hasFocus && !hasSearch);
        if (showLabel) {
          ctx.fillStyle =
            isFocused || isNeighbor || isSearchMatch
              ? "rgba(255, 255, 255, 1)"
              : "rgba(180, 180, 180, 0.8)";
          ctx.font = `${(isFocused ? 12 : 10) / t.k}px Inter, system-ui`;
          ctx.textAlign = "center";
          ctx.fillText(n.label, n.x, n.y - r - 5 / t.k);
        }
      });

      ctx.restore();
    };

    drawRef.current = draw;

    const sim = simulationRef.current;
    // First paint — covers the gap between mount and the first sim
    // tick (especially relevant when the previous frame is still
    // converged and `alpha < alphaMin`).
    draw();

    if (sim) {
      // P6 / M8: hand the render loop to d3-force's internal timer.
      // It auto-advances ticks while alpha >= alphaMin and stops on
      // its own — so the rAF loop that pre-rewrite ran at 60 Hz
      // forever (1–3 % idle CPU) just doesn't exist any more.
      sim.on("tick", draw);
      sim.on("end", draw);
      // If the sim already converged before we mounted (data was
      // unchanged across a re-render), nudge it so the new viewport
      // gets a few warm-up frames.
      if (sim.alpha() < sim.alphaMin()) {
        sim.alpha(0.3).restart();
      }
    }

    return () => {
      drawRef.current = null;
      if (sim) {
        sim.on("tick", null);
        sim.on("end", null);
      }
    };
  }, [nodes, links, neighborMap, size]);

  // 4. Mirror UI state into refs and trigger a single redraw — no
  //    canvas reinit, no sim restart.  This is what makes the search
  //    input feel instant on a 10K-node graph.
  useEffect(() => {
    hoveredRef.current = hovered;
    drawRef.current?.();
  }, [hovered]);
  useEffect(() => {
    isolatedRef.current = isolated;
    drawRef.current?.();
  }, [isolated]);
  useEffect(() => {
    searchQueryRef.current = searchQuery;
    drawRef.current?.();
  }, [searchQuery]);

  // 5. Container resize — guard the destructure: the ResizeObserver
  //    callback can fire with an empty entries array on rare
  //    transition frames during route swaps.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      const { width, height } = entry.contentRect;
      setSize({ w: width, h: height });
    });
    ro.observe(container);
    return () => ro.disconnect();
  }, []);

  // 6. Pan / zoom — keyed on `nodes.length` so the effect re-runs
  //    when the canvas finally mounts after the empty-state branch
  //    falls through.  The ref guard ensures we never re-attach.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || zoomBehaviorRef.current) return;

    const z = zoom<HTMLCanvasElement, unknown>()
      .scaleExtent([0.02, 10])
      .on("zoom", (event) => {
        transformRef.current = event.transform;
        // Force a redraw on user pan/zoom — the simulation may be
        // at rest and ticks won't otherwise fire.
        drawRef.current?.();
      });

    zoomBehaviorRef.current = z;
    select(canvas).call(z);
  }, [nodes.length]);

  // 7. Centre the graph on initial load.
  const centeredRef = useRef(false);
  useEffect(() => {
    if (centeredRef.current || size.w === 0 || nodes.length === 0) return;
    const canvas = canvasRef.current;
    if (!canvas || !zoomBehaviorRef.current) return;

    const initialScale = Math.min(1, 400 / Math.max(200, Math.sqrt(nodes.length) * 30));
    const initialTransform = zoomIdentity
      .translate(size.w / 2, size.h / 2)
      .scale(initialScale);

    select(canvas).call(zoomBehaviorRef.current.transform, initialTransform);
    transformRef.current = initialTransform;
    centeredRef.current = true;
  }, [size, nodes.length]);

  // 8. Hit detection.  Iterates from the top of the z-order down so
  //    the first hit wins — matches the visual stacking order.
  const getHitNode = (e: React.MouseEvent<HTMLCanvasElement>): Node | null => {
    const canvas = canvasRef.current;
    if (!canvas) return null;
    const rect = canvas.getBoundingClientRect();
    const mouseX = e.clientX - rect.left;
    const mouseY = e.clientY - rect.top;

    const t = transformRef.current;
    const virtualX = (mouseX - t.x) / t.k;
    const virtualY = (mouseY - t.y) / t.k;

    for (let i = nodes.length - 1; i >= 0; i--) {
      const n = nodes[i];
      if (!n || n.x == null || n.y == null) continue;
      const dx = n.x - virtualX;
      const dy = n.y - virtualY;
      const r = Math.max(3, Math.min(12, 3 + Math.sqrt(n.claim_count) * 1.5));
      if (dx * dx + dy * dy < (r + 5 / t.k) ** 2) {
        return n;
      }
    }
    return null;
  };

  const handleMouseMove = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    setHovered(hit ? hit.id : null);
  };

  // 9. Click to isolate + recentre.  Pre-rewrite this called
  //    `select(canvas).transition().duration(750).call(...)` but
  //    `d3-transition` was never installed — every click threw
  //    `TypeError: select(...).transition is not a function`, so
  //    the swoop never animated in production anyway.  Instant
  //    recentre is a strict improvement over the broken path.
  const handleClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    if (!hit) {
      setIsolated(null);
      return;
    }
    setIsolated(hit.id);

    const canvas = canvasRef.current;
    if (
      !canvas ||
      !zoomBehaviorRef.current ||
      hit.x == null ||
      hit.y == null
    ) {
      return;
    }

    const scale = Math.max(1.5, transformRef.current.k);
    const targetX = size.w / 2 - hit.x * scale;
    const targetY = size.h / 2 - hit.y * scale;
    const newTransform = zoomIdentity.translate(targetX, targetY).scale(scale);

    select(canvas).call(zoomBehaviorRef.current.transform, newTransform);
    transformRef.current = newTransform;
    drawRef.current?.();
  };

  if (entities.length === 0 && relations.length === 0) {
    return <EmptyGraph />;
  }

  return (
    <div ref={containerRef} className="relative h-full w-full overflow-hidden bg-background">
      <canvas
        ref={canvasRef}
        onMouseMove={handleMouseMove}
        onClick={handleClick}
        onMouseLeave={() => setHovered(null)}
        className="block h-full w-full touch-none outline-none cursor-crosshair"
      />
      <Legend nodeCount={nodes.length} linkCount={links.length} />
    </div>
  );
}

function Legend({ nodeCount, linkCount }: { nodeCount: number; linkCount: number }) {
  return (
    <motion.div
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      className={cn(
        "pointer-events-none absolute bottom-3 left-3 flex items-center gap-3",
        "rounded-md border border-border bg-surface-elevated/95 px-3 py-1.5",
        "text-[10px] text-muted-foreground shadow-pill",
      )}
    >
      <span>
        <span className="font-mono text-foreground">{nodeCount}</span> entities
      </span>
      <span className="text-border">·</span>
      <span>
        <span className="font-mono text-foreground">{linkCount}</span> relations
      </span>
      <span className="text-border">·</span>
      <span>scroll to zoom · drag to pan</span>
    </motion.div>
  );
}

function EmptyGraph() {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="max-w-sm text-center text-sm text-muted-foreground">
        No entities or relations yet. Once your compiled KG has claims
        with entity references, they'll show up as an interactive
        graph here.
      </div>
    </div>
  );
}
