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
import { useBrainActivation, type ActivationKind } from "@/store/brain";

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

// Per-`ActivationKind` halo hue. Mirrors the CSS keyframes in
// `globals.css` (`.brain-pulse-cited` etc.) so a node halo on the
// canvas reads as the same event as a citation chip rendered via
// the matching CSS class. Stable across alpha — opacity is driven by
// the live activation intensity, not by the colour itself.
function activationHue(kind: ActivationKind): { r: number; g: number; b: number } {
  // sky blue / emerald / purple — matches the CSS palette, expressed
  // as RGB so the canvas can blend opacity per intensity.
  switch (kind) {
    case "cited":
      return { r: 100, g: 200, b: 255 }; // sky blue
    case "retrieved":
      return { r: 100, g: 220, b: 160 }; // emerald
    case "cascade":
      return { r: 200, g: 140, b: 255 }; // purple
  }
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

  // Hovered + isolated entities live exclusively in refs (CLAUDE.md
  // audit invariant) — every mouse-move would otherwise re-render
  // this 5K-node component and re-run hook preamble at 60 Hz.  We
  // call `drawRef.current?.()` directly from the event handlers
  // instead, redrawing only the canvas without involving React.
  const hoveredRef = useRef<string | null>(null);
  const isolatedRef = useRef<string | null>(null);
  const searchQueryRef = useRef<string | undefined>(undefined);
  const drawRef = useRef<(() => void) | null>(null);

  // Brain-graph live activity — same refs-not-state posture as
  // hovered/isolated. The activation store is keyed by `claim_id`;
  // BrainGraph derives the per-node halo at draw time using the
  // claim → entity-names resolver built in the data useMemo above.
  // Zustand `subscribe` is what keeps the canvas alive across store
  // updates without re-rendering the whole 5K-node component.
  const activationsRef = useRef<Record<string, { intensity: number; kind: ActivationKind }>>({});
  // Initialised empty; the resolver-mirror effect below pushes the
  // useMemo result in on mount and on every `claims`/`entities` change.
  const claimToEntitiesRef = useRef<Map<string, string[]>>(new Map());
  const decayRafRef = useRef<number | null>(null);
  const setHovered = (id: string | null) => {
    if (hoveredRef.current === id) return;
    hoveredRef.current = id;
    drawRef.current?.();
  };
  const setIsolated = (id: string | null) => {
    if (isolatedRef.current === id) return;
    isolatedRef.current = id;
    drawRef.current?.();
  };

  const [size, setSize] = useState({ w: 800, h: 600 });

  // 1. Prepare data + adjacency map + per-entity best semantic type +
  //    claim → entity-names resolver (the brain-graph activation store
  //    keys by claim id; the canvas needs to know which nodes to halo
  //    when a given claim is cited — same alternation regex pass that
  //    the priority loop runs, so it's free).
  const { nodes, links, neighborMap, claimToEntities } = useMemo(() => {
    const nameToNode = new Map<string, Node>();
    const neighbors = new Map<string, Set<string>>();
    const bestTypeMap = new Map<string, string>();
    const bestRankMap = new Map<string, number>();
    const claimEntityMap = new Map<string, string[]>();

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
        const seenForThisClaim = new Set<string>();
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
          seenForThisClaim.add(name);
        }
        if (seenForThisClaim.size > 0) {
          claimEntityMap.set(claim.id, Array.from(seenForThisClaim));
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
    return {
      nodes: nodeArr,
      links: linkArr,
      neighborMap: neighbors,
      claimToEntities: claimEntityMap,
    };
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

      // Brain-graph live activity halos. Resolves active claim ids
      // into entity-name targets via the cached resolver, then keeps
      // the strongest (intensity, kind) per node so a node cited by
      // multiple claims pulses once at the loudest volume rather than
      // stacking. Drawn last so halos sit visually on top of the node.
      const liveActivations = activationsRef.current;
      if (Object.keys(liveActivations).length > 0) {
        const resolver = claimToEntitiesRef.current;
        const perNode = new Map<string, { intensity: number; kind: ActivationKind }>();
        for (const [claimId, activation] of Object.entries(liveActivations)) {
          const entityNames = resolver.get(claimId);
          if (!entityNames) continue;
          for (const name of entityNames) {
            const prev = perNode.get(name);
            if (!prev || activation.intensity > prev.intensity) {
              perNode.set(name, activation);
            }
          }
        }
        if (perNode.size > 0) {
          ctx.save();
          for (const node of nodes) {
            if (node.x == null || node.y == null) continue;
            const a = perNode.get(node.id);
            if (!a) continue;
            const baseR = Math.max(3, Math.min(12, 3 + Math.sqrt(node.claim_count) * 1.5));
            const haloR = baseR + 8 / t.k + a.intensity * 6;
            const { r: hr, g: hg, b: hb } = activationHue(a.kind);
            const opacity = Math.min(0.85, a.intensity);
            // Outer soft glow.
            const grad = ctx.createRadialGradient(node.x, node.y, baseR, node.x, node.y, haloR);
            grad.addColorStop(0, `rgba(${hr}, ${hg}, ${hb}, ${opacity * 0.55})`);
            grad.addColorStop(1, `rgba(${hr}, ${hg}, ${hb}, 0)`);
            ctx.fillStyle = grad;
            ctx.beginPath();
            ctx.arc(node.x, node.y, haloR, 0, 2 * Math.PI);
            ctx.fill();
            // Crisp inner ring so the activation is legible even
            // against a busy background.
            ctx.beginPath();
            ctx.arc(node.x, node.y, baseR + 2 / t.k, 0, 2 * Math.PI);
            ctx.strokeStyle = `rgba(${hr}, ${hg}, ${hb}, ${opacity})`;
            ctx.lineWidth = 1.5 / t.k;
            ctx.stroke();
          }
          ctx.restore();
        }
      }

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

  // 4. Mirror only `searchQuery` (a parent prop, so we can't make it
  //    a ref) into the canvas-readable ref. Hovered/isolated already
  //    sit in refs — see the `setHovered` / `setIsolated` shims
  //    above — so no extra effect is needed for them.
  useEffect(() => {
    searchQueryRef.current = searchQuery;
    drawRef.current?.();
  }, [searchQuery]);

  // 4b. Keep the claim→entity resolver ref in lockstep with the
  //     useMemo result. The canvas draw closure reads it without
  //     re-binding, so a new resolver shape just lands silently on
  //     the next frame.
  useEffect(() => {
    claimToEntitiesRef.current = claimToEntities;
  }, [claimToEntities]);

  // 4c. Subscribe to the brain-activation store + drive an
  //     exponential-decay rAF loop while any claim is still
  //     activated. The store applies the decay; we just keep ticking
  //     until it returns an empty map and then stop the loop so the
  //     canvas goes back to 0% CPU (the same posture as the d3-force
  //     `on("end")` boundary).
  useEffect(() => {
    const tick = () => {
      const now = performance.now();
      useBrainActivation.getState().decay(now);
      const live = useBrainActivation.getState().activations;
      activationsRef.current = Object.fromEntries(
        Object.entries(live).map(([id, a]) => [id, { intensity: a.intensity, kind: a.kind }]),
      );
      drawRef.current?.();
      if (Object.keys(live).length > 0) {
        decayRafRef.current = requestAnimationFrame(tick);
      } else {
        decayRafRef.current = null;
      }
    };

    const unsubscribe = useBrainActivation.subscribe((state) => {
      // Mirror the latest activations into the ref synchronously so
      // a draw triggered by some other path (hover, zoom) sees them
      // without waiting for the next rAF tick.
      activationsRef.current = Object.fromEntries(
        Object.entries(state.activations).map(([id, a]) => [
          id,
          { intensity: a.intensity, kind: a.kind },
        ]),
      );
      drawRef.current?.();
      if (
        Object.keys(state.activations).length > 0 &&
        decayRafRef.current === null
      ) {
        decayRafRef.current = requestAnimationFrame(tick);
      }
    });

    return () => {
      unsubscribe();
      if (decayRafRef.current !== null) {
        cancelAnimationFrame(decayRafRef.current);
        decayRafRef.current = null;
      }
    };
  }, []);

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
