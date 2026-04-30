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

/**
 * Obsidian-Grade Premium Canvas Graph.
 * True Retina (DPR) resolution, GPU-accelerated rendering.
 * Features organic physics flow, search syncing, semantic colors, and exact hit-detection.
 */
export function BrainGraph({ entities, relations, claims = [], searchQuery }: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const simulationRef = useRef<any>(null);
  const transformRef = useRef<ZoomTransform>(zoomIdentity);
  const zoomBehaviorRef = useRef<ZoomBehavior<HTMLCanvasElement, unknown> | null>(null);
  
  const [hovered, setHovered] = useState<string | null>(null);
  const [isolated, setIsolated] = useState<string | null>(null);
  const [size, setSize] = useState({ w: 800, h: 600 });

  // 1. Prepare data & Fast Adjacency Map
  const { nodes, links, neighborMap } = useMemo(() => {
    const nameToNode = new Map<string, Node>();
    const neighbors = new Map<string, Set<string>>();

    // Pre-calculate best semantic type based on claims
    const bestTypeMap = new Map<string, string>();
    for (const claim of claims) {
      for (const e of entities) {
        if (claim.statement.includes(e.name)) {
          // Priority: Definition > ApiSignature > Architecture > Requirement > others
          const current = bestTypeMap.get(e.name);
          const incoming = claim.claim_type;
          
          if (!current) {
            bestTypeMap.set(e.name, incoming);
          } else {
            const priority = ["definition", "apisignature", "architecture", "requirement", "fact"];
            const currentIdx = priority.indexOf(current.toLowerCase());
            const incomingIdx = priority.indexOf(incoming.toLowerCase());
            
            if (incomingIdx !== -1 && (currentIdx === -1 || incomingIdx < currentIdx)) {
              bestTypeMap.set(e.name, incoming);
            }
          }
          
          // Also set tier if rooted
          if (claim.tier === "rooted" && (!current || current.toLowerCase() !== "definition")) {
             bestTypeMap.set(e.name, "rooted");
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

  // 2. Initialize Premium Physics Engine
  useEffect(() => {
    if (nodes.length === 0) return;

    // Organic "Flow" settings: 
    const sim = forceSimulation<Node>(nodes)
      .alphaDecay(0.015) 
      .force("link", forceLink<Node, Link>(links).id((d) => d.id).distance(60).strength(0.5))
      .force("charge", forceManyBody().strength(-150))
      .force("center", forceCenter(0, 0))
      .force("collide", forceCollide<Node>().radius(d => 4 + Math.sqrt(d.claim_count) * 1.5).iterations(2));

    simulationRef.current = sim;

    return () => {
      sim.stop();
    };
  }, [nodes, links]);

  // 3. High-Fidelity Retina Render Loop
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

    let animationId: number;

    const render = () => {
      ctx.save();
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.scale(dpr, dpr);
      
      const t = transformRef.current;
      ctx.translate(t.x, t.y);
      ctx.scale(t.k, t.k);

      const activeFocus = hovered || isolated;
      const activeNeighbors = activeFocus ? neighborMap.get(activeFocus) || new Set() : new Set();
      const hasFocus = activeFocus !== null;
      
      const q = searchQuery?.trim().toLowerCase();
      const hasSearch = !!q;

      // Draw Lines
      ctx.beginPath();
      links.forEach((l) => {
        const s = l.source as Node;
        const targetNode = l.target as Node;
        if (s.x != null && targetNode.x != null) {
          const isRelated = activeFocus && (s.id === activeFocus || targetNode.id === activeFocus);
          
          ctx.beginPath();
          ctx.moveTo(s.x, s.y!);
          ctx.lineTo(targetNode.x, targetNode.y!);
          
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

      // Draw Nodes
      nodes.forEach((n) => {
        if (n.x == null || n.y == null) return;
        
        const r = Math.max(3, Math.min(12, 3 + Math.sqrt(n.claim_count) * 1.5));
        
        const isFocused = activeFocus === n.id;
        const isNeighbor = activeNeighbors.has(n.id);
        const isSearchMatch = hasSearch && n.label.toLowerCase().includes(q!);

        if (isFocused || isSearchMatch) {
          ctx.beginPath();
          ctx.arc(n.x, n.y, r + 6 / t.k, 0, 2 * Math.PI);
          ctx.fillStyle = isSearchMatch && !isFocused ? "rgba(255, 200, 100, 0.25)" : "rgba(100, 200, 255, 0.25)";
          ctx.fill();
        }

        ctx.beginPath();
        ctx.arc(n.x, n.y, r, 0, 2 * Math.PI);
        
        if (isFocused) {
          ctx.fillStyle = "rgb(255, 255, 255)"; 
        } else if (isNeighbor) {
          ctx.fillStyle = "rgb(100, 200, 255)"; 
        } else if (isSearchMatch) {
          ctx.fillStyle = "rgb(255, 200, 100)"; // Search highlight (amber)
        } else {
          // Semantic color if not dimmed
          const semanticColor = getSemanticColor(n.entity_type);
          if (hasFocus || hasSearch) {
             ctx.fillStyle = "rgba(100, 100, 100, 0.1)"; // Highly dimmed
          } else {
             ctx.fillStyle = semanticColor;
          }
        }
        ctx.fill();

        // Crisp Labels
        const showLabel = isFocused || isNeighbor || isSearchMatch || (nodes.length < 100 && !hasFocus && !hasSearch);
        if (showLabel) {
          ctx.fillStyle = isFocused || isNeighbor || isSearchMatch ? "rgba(255, 255, 255, 1)" : "rgba(180, 180, 180, 0.8)";
          ctx.font = `${(isFocused ? 12 : 10) / t.k}px Inter, system-ui`;
          ctx.textAlign = "center";
          ctx.fillText(n.label, n.x, n.y - r - (5 / t.k));
        }
      });

      ctx.restore();
      animationId = requestAnimationFrame(render);
    };

    animationId = requestAnimationFrame(render);
    return () => cancelAnimationFrame(animationId);
  }, [nodes, links, neighborMap, hovered, isolated, size, searchQuery]);

  // 4. Stable Resizing
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver((entries) => {
      const { width, height } = entries[0].contentRect;
      setSize({ w: width, h: height });
    });
    ro.observe(container);
    return () => ro.disconnect();
  }, []);

  // 5. Smooth Pan/Zoom — keyed on nodes.length so the effect re-runs when
  // the canvas finally mounts after the empty-state branch falls through.
  // Guarded by the ref so we never re-attach behavior on data refreshes.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || zoomBehaviorRef.current) return;

    const z = zoom<HTMLCanvasElement, unknown>()
      .scaleExtent([0.02, 10])
      .on("zoom", (event) => {
        transformRef.current = event.transform;
      });

    zoomBehaviorRef.current = z;
    select(canvas).call(z);
  }, [nodes.length]);

  // Center Graph on initial load
  const centeredRef = useRef(false);
  useEffect(() => {
    if (centeredRef.current || size.w === 0 || nodes.length === 0) return;
    const canvas = canvasRef.current;
    if (!canvas || !zoomBehaviorRef.current) return;

    const initialScale = Math.min(1, 400 / Math.max(200, Math.sqrt(nodes.length) * 30));
    const initialTransform = zoomIdentity
      .translate(size.w / 2, size.h / 2)
      .scale(initialScale);

    select(canvas).call(zoomBehaviorRef.current.transform as any, initialTransform);
    transformRef.current = initialTransform;
    centeredRef.current = true;
  }, [size, nodes.length]);

  // Hit Detection Logic
  const getHitNode = (e: React.MouseEvent<HTMLCanvasElement>) => {
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
      if (n.x == null || n.y == null) continue;
      
      const dx = n.x - virtualX;
      const dy = n.y - virtualY;
      const r = Math.max(3, Math.min(12, 3 + Math.sqrt(n.claim_count) * 1.5));
      
      if (dx * dx + dy * dy < (r + (5 / t.k)) ** 2) {
        return n;
      }
    }
    return null;
  };

  const handleMouseMove = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    setHovered(hit ? hit.id : null);
  };

  // 6. Click to Isolate & Swoop Zoom
  const handleClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    if (!hit) {
      setIsolated(null); // Clicked empty space
      return;
    }
    
    setIsolated(hit.id);

    const canvas = canvasRef.current;
    if (!canvas || !zoomBehaviorRef.current) return;

    // Transition camera to center on the clicked node
    const scale = Math.max(1.5, transformRef.current.k); // Zoom in slightly if far out
    const targetX = size.w / 2 - hit.x! * scale;
    const targetY = size.h / 2 - hit.y! * scale;
    
    const newTransform = zoomIdentity.translate(targetX, targetY).scale(scale);

    select(canvas)
      .transition()
      .duration(750) // Premium cinematic swoop duration
      .call(zoomBehaviorRef.current.transform as any, newTransform);
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
