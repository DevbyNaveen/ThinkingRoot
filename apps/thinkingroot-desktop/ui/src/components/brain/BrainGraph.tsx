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
import { select } from "d3-selection";
import { zoom, type ZoomBehavior } from "d3-zoom";
import { motion } from "framer-motion";
import type { BrainEntity, BrainRelation } from "@/lib/tauri";
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
}

/**
 * Force-directed graph of entities + relations. Renders to SVG so
 * the nodes inherit theme-variable fills (dark / light / daltonized)
 * without a paint path through `<canvas>`.
 *
 * d3-force runs the simulation every frame; we re-read node
 * coordinates into component state so React re-paints circles +
 * lines at the correct positions. On unmount the simulation stops.
 */
export function BrainGraph({ entities, relations }: Props) {
  const svgRef = useRef<SVGSVGElement | null>(null);
  const gRef = useRef<SVGGElement | null>(null);
  const zoomRef = useRef<ZoomBehavior<SVGSVGElement, unknown> | null>(null);
  const [size, setSize] = useState({ w: 640, h: 480 });
  const [tick, setTick] = useState(0);
  const [hovered, setHovered] = useState<string | null>(null);

  // Resize observer
  useEffect(() => {
    const el = svgRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const r = el.getBoundingClientRect();
      setSize({ w: Math.max(400, r.width), h: Math.max(300, r.height) });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Build the force simulation.
  const { nodes, links } = useMemo(() => {
    const nameToNode = new Map<string, Node>();
    for (const e of entities) {
      nameToNode.set(e.name, {
        id: e.name,
        label: e.name,
        claim_count: e.claim_count,
        entity_type: e.entity_type,
      });
    }
    // Include relation endpoints that aren't first-class entities.
    for (const r of relations) {
      for (const name of [r.source, r.target]) {
        if (!nameToNode.has(name)) {
          nameToNode.set(name, {
            id: name,
            label: name,
            claim_count: 0,
            entity_type: "inferred",
          });
        }
      }
    }
    const nodeArr = Array.from(nameToNode.values());
    const linkArr: Link[] = relations.map((r) => ({
      source: r.source,
      target: r.target,
      type: r.relation_type,
      strength: r.strength,
    }));
    return { nodes: nodeArr, links: linkArr };
  }, [entities, relations]);

  useEffect(() => {
    if (nodes.length === 0) return;
    const sim = forceSimulation<Node>(nodes)
      .force(
        "link",
        forceLink<Node, Link>(links)
          .id((d) => d.id)
          .distance(70)
          .strength(0.6),
      )
      .force("charge", forceManyBody().strength(-180))
      .force("center", forceCenter(size.w / 2, size.h / 2))
      .force("collide", forceCollide(22));

    sim.stop(); // Take complete manual control over ticks

    let stopped = false;
    let i = 0;
    const totalTicks = Math.ceil(
      Math.log(sim.alphaMin()) / Math.log(1 - sim.alphaDecay())
    );

    function step() {
      if (stopped) return;
      const start = performance.now();

      // Chunk physics calculations: perform up to 12ms of work per frame.
      // This strictly avoids locking the main thread and hanging the UI.
      while (performance.now() - start < 12 && i < totalTicks) {
        sim.tick();
        i++;
      }

      if (i < totalTicks) {
        // If graph is huge, heavily throttle React re-renders so the DOM
        // doesn't burn CPU reconstructing 7000 SVG elements every 16ms.
        if (nodes.length < 500 || i % 15 === 0) {
          setTick((t) => (t + 1) % 1_000_000);
        }
        requestAnimationFrame(step);
      } else {
        // Run a final render when stable
        setTick((t) => t + 1);
      }
    }

    requestAnimationFrame(step);

    return () => {
      stopped = true;
      sim.stop();
    };
  }, [nodes, links, size.w, size.h]);

  // Wire pan/zoom once we have an SVG + group handle.
  useEffect(() => {
    const svgEl = svgRef.current;
    const gEl = gRef.current;
    if (!svgEl || !gEl) return;

    const z = zoom<SVGSVGElement, unknown>()
      .scaleExtent([0.3, 4])
      .on("zoom", (event) => {
        select(gEl).attr("transform", String(event.transform));
      });
    zoomRef.current = z;
    select(svgEl).call(z);
  }, []);

  const tierColor = (confidence: number, rooted: boolean) =>
    rooted
      ? "var(--tier-rooted-hsl, hsl(var(--tier-rooted)))"
      : confidence >= 0.7
        ? "var(--tier-attested-hsl, hsl(var(--tier-attested)))"
        : "var(--tier-unknown-hsl, hsl(var(--tier-unknown)))";

  if (entities.length === 0 && relations.length === 0) {
    return <EmptyGraph />;
  }

  return (
    <div className="relative h-full w-full overflow-hidden">
      <svg
        ref={svgRef}
        role="img"
        aria-label="Knowledge graph"
        className="absolute inset-0 h-full w-full"
      >
        <g ref={gRef}>
          <g aria-label="Relations">
            {links.map((l, i) => {
              const s = l.source as Node;
              const t = l.target as Node;
              if (!s || !t || s.x == null || t.x == null) return null;
              const isHoverRelated =
                hovered && (s.id === hovered || t.id === hovered);
              return (
                <line
                  key={`l-${i}`}
                  x1={s.x}
                  y1={s.y}
                  x2={t.x}
                  y2={t.y}
                  stroke="hsl(var(--border))"
                  strokeOpacity={isHoverRelated ? 0.9 : 0.35}
                  strokeWidth={isHoverRelated ? 1.2 : 0.8}
                />
              );
            })}
          </g>
          <g aria-label="Entities">
            {nodes.map((n) => {
              if (n.x == null || n.y == null) return null;
              const r = Math.max(5, Math.min(14, 5 + Math.sqrt(n.claim_count)));
              const isHovered = hovered === n.id;
              return (
                <g
                  key={n.id}
                  transform={`translate(${n.x},${n.y})`}
                  onMouseEnter={() => setHovered(n.id)}
                  onMouseLeave={() => setHovered((h) => (h === n.id ? null : h))}
                >
                  <circle
                    r={r}
                    fill={tierColor(0.5, false)}
                    fillOpacity={isHovered ? 0.85 : 0.55}
                    stroke="hsl(var(--background))"
                    strokeWidth={1.5}
                    className="cursor-pointer transition-[fill-opacity]"
                  />
                  {isHovered && (
                    <text
                      y={-r - 6}
                      textAnchor="middle"
                      className="pointer-events-none fill-foreground text-[10px] font-medium"
                    >
                      {n.label}
                    </text>
                  )}
                </g>
              );
            })}
          </g>
        </g>
      </svg>

      <Legend nodeCount={nodes.length} linkCount={links.length} />
      {/* force a re-render on each sim tick — React reads node positions via refs held on `nodes` objects, so we just need a lifecycle ping */}
      <span className="sr-only">frame {tick}</span>
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
        force-directed graph here.
      </div>
    </div>
  );
}
