/**
 * Brain-graph canvas — worker-backed, smooth at 10k+ nodes.
 *
 * Architecture (every piece is here on purpose):
 *
 *   • Two Web Workers — `entityResolver` (regex/matchAll for semantic
 *     types + claim→entity resolver) and `forceLayout` (d3-force loop
 *     streaming Float32Array position deltas).  Both load via the
 *     standard Vite `new Worker(new URL(...), { type: "module" })`
 *     idiom so HMR + bundling Just Work.
 *
 *   • Positions live in a `Float32Array` ref — never in React state.
 *     Every worker tick flips the ref to a new buffer (transferable,
 *     zero-copy) and asks the canvas to redraw.  React never sees the
 *     tick rate.
 *
 *   • Layout persistence via `graphLayoutPersist` — node positions
 *     and zoom transform survive page reloads.  When the persisted
 *     fingerprint matches the current entity set we start with
 *     `alpha = 0.05` (just enough to settle sub-pixel drift) instead
 *     of `alpha = 1` (full 5–15s freeze on a 10k-node graph).
 *
 *   • Viewport + LOD culling — at zoom < threshold we skip labels
 *     and very-small-radius nodes; offscreen nodes are skipped in
 *     both the link loop and the node loop.  Keeps pan/zoom at 60 fps
 *     even before the simulation cools.
 *
 *   • Visibility pause — when `isVisible` is false the layout worker
 *     receives `pause` and the activation rAF stops.  Coming back
 *     into view sends `resume`, which restarts d3-force at the
 *     current alpha (typically 0, instant continue).
 *
 *   • Activation halos — unchanged behaviour: the chat token stream
 *     drives `useBrainActivation`, the resolver Map sent by the
 *     entity-resolver worker maps a `claim_id` onto entity-node
 *     halos.  Decay loop is still rAF-driven (lightweight; no DOM
 *     mutation, only canvas paint).
 *
 * Honesty notes:
 *
 *   - When the entity-resolver hasn't responded yet, nodes fall back
 *     to `entity.entity_type` (the engine's mechanical type) so the
 *     graph never reads as empty.  The semantic upgrade lands when
 *     the worker reply arrives and triggers a single redraw.
 *
 *   - The persisted positions are advisory.  A schema mismatch, a
 *     QuotaExceeded error, or a workspace that simply hasn't been
 *     opened before all fall through to a fresh `alpha=1` layout —
 *     never to a broken canvas.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { zoom, zoomIdentity, type ZoomBehavior, type ZoomTransform } from "d3-zoom";
import { select } from "d3-selection";
import type { BrainEntity, BrainRelation, ClaimRow } from "@/lib/tauri";
import { cn } from "@/lib/utils";
import { useBrainActivation, type ActivationKind } from "@/store/brain";
import {
  fingerprintEntities,
  loadGraphLayout,
  saveGraphLayout,
} from "@/lib/graphLayoutPersist";
import type {
  EntityResolverRequest,
  EntityResolverResponse,
} from "@/workers/entityResolver.worker";

// ───────────────────────── Component contract ─────────────────────────

interface Props {
  entities: BrainEntity[];
  relations: BrainRelation[];
  claims?: ClaimRow[];
  searchQuery?: string;
  cacheKey?: string;
  /** When false the simulation pauses.  Defaults to true. */
  isVisible?: boolean;
}

// ───────────────────────── Internal shapes ────────────────────────────

interface NodeMeta {
  id: string;
  label: string;
  claim_count: number;
  entity_type: string;
}

interface WorkerLinkOut {
  source: string;
  target: string;
  strength: number;
}

interface InternalLink {
  sourceIdx: number;
  targetIdx: number;
  type: string;
  strength: number;
}

// ───────────────────────── Helpers ────────────────────────────────────

// Production palette covering every value `node.entity_type` can take.
// Two source vocabularies feed this string:
//   1. `ClaimType` (worker-resolved; entityResolver.worker.ts) — serde
//      snake_case from `crates/thinkingroot-core/src/types/claim.rs`.
//   2. `EntityType` (backend fallback) — serde snake_case from
//      `crates/thinkingroot-core/src/types/entity.rs`.
// Plus two synthetic strings: `"rooted"` (worker override when any
// matching claim is tier=rooted) and `"inferred"` (BrainGraph.tsx
// fallback for entities mentioned in relations but absent from the
// entity list). Lightness pinned 55–72% for legibility on the dark
// canvas; hue separation ≥ 25° between same-vocabulary neighbours.
const SEMANTIC_PALETTE: Readonly<Record<string, string>> = {
  // ── ClaimType (snake_case wire) ──
  fact: "hsl(215, 55%, 65%)",
  decision: "hsl(45, 75%, 60%)",
  opinion: "hsl(15, 60%, 68%)",
  plan: "hsl(135, 50%, 58%)",
  requirement: "hsl(340, 70%, 65%)",
  metric: "hsl(180, 70%, 55%)",
  definition: "hsl(280, 70%, 65%)",
  dependency: "hsl(305, 55%, 62%)",
  api_signature: "hsl(200, 80%, 65%)",
  // Legacy compact spelling — kept as a no-cost alias in case any
  // upstream path emits `apisignature` without the underscore.
  apisignature: "hsl(200, 80%, 65%)",
  architecture: "hsl(30, 80%, 65%)",
  preference: "hsl(60, 55%, 60%)",
  // ── EntityType (snake_case wire) ──
  person: "hsl(0, 65%, 68%)",
  system: "hsl(250, 50%, 65%)",
  service: "hsl(160, 55%, 55%)",
  concept: "hsl(265, 55%, 72%)",
  team: "hsl(325, 50%, 68%)",
  api: "hsl(195, 70%, 60%)",
  database: "hsl(85, 50%, 55%)",
  library: "hsl(290, 55%, 68%)",
  file: "hsl(45, 70%, 62%)",
  module: "hsl(225, 60%, 65%)",
  function: "hsl(170, 60%, 55%)",
  config: "hsl(220, 22%, 60%)",
  organization: "hsl(20, 55%, 62%)",
  // ── Synthetic ──
  rooted: "hsl(150, 70%, 60%)",
  inferred: "rgba(140, 140, 140, 0.4)",
};

// Dim slate — deliberately not pale (`rgba(200,200,200,0.8)` reads as
// white on the dark canvas). Used only when an unknown type arrives;
// the palette above is meant to cover every real wire value.
const SEMANTIC_DEFAULT = "hsl(220, 15%, 58%)";

function getSemanticColor(type: string): string {
  return SEMANTIC_PALETTE[type.toLowerCase()] ?? SEMANTIC_DEFAULT;
}

function activationHue(kind: ActivationKind): { r: number; g: number; b: number } {
  switch (kind) {
    case "cited":
      return { r: 100, g: 200, b: 255 };
    case "retrieved":
      return { r: 100, g: 220, b: 160 };
    case "cascade":
      return { r: 200, g: 140, b: 255 };
  }
}

function nodeRadius(claimCount: number): number {
  return Math.max(3, Math.min(12, 3 + Math.sqrt(claimCount) * 1.5));
}

// ───────────────────────── Component ──────────────────────────────────

export function BrainGraph({
  entities,
  relations,
  claims = [],
  searchQuery,
  cacheKey,
  isVisible = true,
}: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);

  // d3-zoom plumbing
  const transformRef = useRef<ZoomTransform>(zoomIdentity);
  const zoomBehaviorRef = useRef<ZoomBehavior<HTMLCanvasElement, unknown> | null>(null);

  // Hover / isolate / search — refs so a mouse-move doesn't re-render.
  const hoveredRef = useRef<string | null>(null);
  const isolatedRef = useRef<string | null>(null);
  const searchQueryRef = useRef<string | undefined>(undefined);
  const drawRef = useRef<(() => void) | null>(null);

  // Position state (worker-driven, never via React).
  const positionsRef = useRef<Float32Array | null>(null);
  const idIndexRef = useRef<Map<string, number>>(new Map());

  // Activation overlay refs (citation halos).
  const activationsRef = useRef<Record<string, { intensity: number; kind: ActivationKind }>>({});
  const claimToEntitiesRef = useRef<Map<string, string[]>>(new Map());
  const decayRafRef = useRef<number | null>(null);

  // Per-node semantic-type upgrade from the entity-resolver worker.
  const [bestTypeMap, setBestTypeMap] = useState<Map<string, string>>(() => new Map());

  const [size, setSize] = useState({ w: 800, h: 600 });

  // Note: `persistedHint` is computed lower down, once `fingerprint`
  // is available from the `nodes` useMemo.

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

  // ── 1. Build nodes + links synchronously (no regex pass) ────────────
  //
  // The regex/matchAll work happens off the main thread in
  // `entityResolver.worker.ts`.  Here we just produce the structural
  // shape — nodes from `entities`, links from `relations`.  Semantic
  // types start at the engine-provided `entity.entity_type` and get
  // upgraded asynchronously when the worker reply lands.
  const { nodes, links, neighborMap, fingerprint } = useMemo(() => {
    const nameToIndex = new Map<string, number>();
    const nodeArr: NodeMeta[] = [];

    for (const e of entities) {
      nameToIndex.set(e.name, nodeArr.length);
      nodeArr.push({
        id: e.name,
        label: e.name,
        claim_count: e.claim_count,
        entity_type: bestTypeMap.get(e.name) ?? e.entity_type,
      });
    }
    // Relations may reference entity names that aren't in `entities`
    // (e.g. structural-only relations from a partial compile).  Add
    // them as inferred-type nodes so the link draws correctly.
    for (const r of relations) {
      for (const name of [r.source, r.target]) {
        if (!nameToIndex.has(name)) {
          nameToIndex.set(name, nodeArr.length);
          nodeArr.push({
            id: name,
            label: name,
            claim_count: 0,
            entity_type: bestTypeMap.get(name) ?? "inferred",
          });
        }
      }
    }

    const linkArr: InternalLink[] = [];
    const neighbors = new Map<number, Set<number>>();
    for (const r of relations) {
      const s = nameToIndex.get(r.source);
      const t = nameToIndex.get(r.target);
      if (s === undefined || t === undefined) continue;
      linkArr.push({
        sourceIdx: s,
        targetIdx: t,
        type: r.relation_type,
        strength: r.strength,
      });
      if (!neighbors.has(s)) neighbors.set(s, new Set());
      if (!neighbors.has(t)) neighbors.set(t, new Set());
      neighbors.get(s)!.add(t);
      neighbors.get(t)!.add(s);
    }

    idIndexRef.current = nameToIndex;

    return {
      nodes: nodeArr,
      links: linkArr,
      neighborMap: neighbors,
      fingerprint: fingerprintEntities(nodeArr.map((n) => n.id)),
    };
  }, [entities, relations, bestTypeMap]);

  // Persisted layout, loaded once per (workspace, fingerprint) pair.
  // Both the layout-init effect and the centering effect read this —
  // doing it via `useMemo` avoids cross-effect timing races AND avoids
  // parsing the ~250 KB localStorage payload every time `bestTypeMap`
  // updates (which rebuilds `nodes` but doesn't change the id set).
  const persistedHint = useMemo(() => {
    return cacheKey ? loadGraphLayout(cacheKey, fingerprint) : null;
  }, [cacheKey, fingerprint]);

  // ── 2. Entity-resolver worker — semantic type upgrade ───────────────
  const resolverWorkerRef = useRef<Worker | null>(null);
  const resolverReqIdRef = useRef<number>(0);

  useEffect(() => {
    const worker = new Worker(
      new URL("../../workers/entityResolver.worker.ts", import.meta.url),
      { type: "module" },
    );
    resolverWorkerRef.current = worker;

    worker.onmessage = (e: MessageEvent<EntityResolverResponse>) => {
      const { reqId, bestType, claimToEntities } = e.data;
      // Drop stale replies — only the latest request's result counts.
      if (reqId !== resolverReqIdRef.current) return;
      setBestTypeMap(new Map(bestType));
      claimToEntitiesRef.current = new Map(claimToEntities);
      drawRef.current?.();
    };

    return () => {
      worker.terminate();
      resolverWorkerRef.current = null;
    };
  }, []);

  useEffect(() => {
    const worker = resolverWorkerRef.current;
    if (!worker) return;
    resolverReqIdRef.current += 1;
    const reqId = resolverReqIdRef.current;
    const req: EntityResolverRequest = {
      reqId,
      entities: entities.map((e) => ({
        name: e.name,
        entity_type: e.entity_type,
        claim_count: e.claim_count,
      })),
      claims: claims.map((c) => ({
        id: c.id,
        statement: c.statement,
        claim_type: c.claim_type,
        tier: c.tier,
      })),
    };
    worker.postMessage(req);
  }, [entities, claims]);

  // ── 3. Force-layout worker + position stream ────────────────────────
  const layoutWorkerRef = useRef<Worker | null>(null);
  const hasInitedLayoutRef = useRef<boolean>(false);

  useEffect(() => {
    const worker = new Worker(
      new URL("../../workers/forceLayout.worker.ts", import.meta.url),
      { type: "module" },
    );
    layoutWorkerRef.current = worker;

    worker.onmessage = (
      e: MessageEvent<{
        type: "tick" | "end";
        positions: Float32Array;
        alpha: number;
        ids?: string[];
      }>,
    ) => {
      const msg = e.data;
      positionsRef.current = msg.positions;
      if (msg.ids) {
        const map = new Map<string, number>();
        for (let i = 0; i < msg.ids.length; i++) {
          const name = msg.ids[i];
          if (name !== undefined) map.set(name, i);
        }
        idIndexRef.current = map;
      }
      drawRef.current?.();

      if (msg.type === "end") {
        // Persist the converged layout so the next session warm-starts.
        const positions = positionsRef.current;
        const ids = msg.ids;
        if (positions && ids && cacheKey) {
          const out = new Map<string, { x: number; y: number }>();
          for (let i = 0; i < ids.length; i++) {
            const name = ids[i];
            if (name === undefined) continue;
            const x = positions[i * 2];
            const y = positions[i * 2 + 1];
            if (x === undefined || y === undefined) continue;
            out.set(name, { x, y });
          }
          const t = transformRef.current;
          saveGraphLayout(cacheKey, fingerprint, out, {
            x: t.x,
            y: t.y,
            k: t.k,
          });
        }
      }
    };

    return () => {
      worker.postMessage({ type: "stop" });
      worker.terminate();
      layoutWorkerRef.current = null;
      hasInitedLayoutRef.current = false;
    };
  }, [cacheKey, fingerprint]);

  // Send init/update to the layout worker when the graph shape changes.
  //
  // An exact fingerprint match on the persisted layout means we can
  // warm-start at near-zero alpha; a partial match (some entities
  // carry over, others are new) still uses the surviving positions
  // but cools with normal alpha so new nodes settle.
  useEffect(() => {
    const worker = layoutWorkerRef.current;
    if (!worker || nodes.length === 0) return;

    const startingPositions = persistedHint?.positions ?? null;
    const nodesForWorker = nodes.map((n) => {
      const carried = startingPositions?.get(n.id);
      return {
        id: n.id,
        claim_count: n.claim_count,
        x: carried?.x,
        y: carried?.y,
      };
    });
    const linksForWorker: WorkerLinkOut[] = links.map((l) => {
      const source = nodes[l.sourceIdx];
      const target = nodes[l.targetIdx];
      return {
        source: source?.id ?? "",
        target: target?.id ?? "",
        strength: l.strength,
      };
    });

    const alpha = persistedHint?.fingerprintMatches ? 0.05 : 1;

    if (!hasInitedLayoutRef.current) {
      worker.postMessage({
        type: "init",
        nodes: nodesForWorker,
        links: linksForWorker,
        alpha,
      });
      hasInitedLayoutRef.current = true;
    } else {
      worker.postMessage({
        type: "update",
        nodes: nodesForWorker,
        links: linksForWorker,
        alpha,
      });
    }
  }, [nodes, links, persistedHint]);

  // Pause/resume when isVisible flips.
  useEffect(() => {
    const worker = layoutWorkerRef.current;
    if (!worker) return;
    worker.postMessage({ type: isVisible ? "resume" : "pause" });
  }, [isVisible]);

  // ── 4. Canvas + draw ────────────────────────────────────────────────
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
      const positions = positionsRef.current;
      ctx.save();
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.scale(dpr, dpr);

      const t = transformRef.current;
      ctx.translate(t.x, t.y);
      ctx.scale(t.k, t.k);

      // Compute visible bounds in virtual space — anything outside is
      // culled.  Cheap rectangle vs single AABB per node.
      const viewMinX = -t.x / t.k;
      const viewMinY = -t.y / t.k;
      const viewMaxX = viewMinX + size.w / t.k;
      const viewMaxY = viewMinY + size.h / t.k;

      const hoveredId = hoveredRef.current;
      const isolatedId = isolatedRef.current;
      const activeFocusId = hoveredId ?? isolatedId;
      const activeFocusIdx =
        activeFocusId !== null ? (idIndexRef.current.get(activeFocusId) ?? -1) : -1;
      const activeNeighbors =
        activeFocusIdx >= 0
          ? (neighborMap.get(activeFocusIdx) ?? new Set<number>())
          : new Set<number>();
      const hasFocus = activeFocusIdx >= 0;

      const q = searchQueryRef.current?.trim().toLowerCase();
      const hasSearch = !!q;

      // LOD: when zoomed out far OR the graph is large and unfocused,
      // skip labels except for matched / focused nodes.  Empirically
      // ~3000 nodes is where the per-label text rasterisation starts
      // to dominate at 60 fps on M-series.
      const lodHideLabels = t.k < 0.6 || (nodes.length > 3000 && !hasFocus && !hasSearch);

      // ── Links pass ───────────────────────────────────────────────
      if (positions) {
        ctx.lineCap = "round";
        for (const l of links) {
          const sx = positions[l.sourceIdx * 2];
          const sy = positions[l.sourceIdx * 2 + 1];
          const tx = positions[l.targetIdx * 2];
          const ty = positions[l.targetIdx * 2 + 1];
          if (sx === undefined || sy === undefined || tx === undefined || ty === undefined) {
            continue;
          }

          // Viewport cull on the bounding box of the segment.
          const minX = Math.min(sx, tx);
          const maxX = Math.max(sx, tx);
          const minY = Math.min(sy, ty);
          const maxY = Math.max(sy, ty);
          if (maxX < viewMinX || minX > viewMaxX) continue;
          if (maxY < viewMinY || minY > viewMaxY) continue;

          const isRelated =
            hasFocus && (l.sourceIdx === activeFocusIdx || l.targetIdx === activeFocusIdx);

          ctx.beginPath();
          ctx.moveTo(sx, sy);
          ctx.lineTo(tx, ty);
          if (isRelated) {
            ctx.strokeStyle = "rgba(100, 200, 255, 0.7)";
            ctx.lineWidth = 1.5 / t.k;
          } else {
            const opacity = hasFocus || hasSearch ? 0.02 : 0.08;
            ctx.strokeStyle = `rgba(255, 255, 255, ${opacity})`;
            ctx.lineWidth = 0.8 / t.k;
          }
          ctx.stroke();
        }
      }

      // ── Nodes pass ───────────────────────────────────────────────
      if (positions) {
        for (let i = 0; i < nodes.length; i++) {
          const n = nodes[i];
          if (!n) continue;
          const x = positions[i * 2];
          const y = positions[i * 2 + 1];
          if (x === undefined || y === undefined) continue;

          const r = nodeRadius(n.claim_count);
          // Viewport cull — include radius + halo margin.
          if (x + r < viewMinX || x - r > viewMaxX) continue;
          if (y + r < viewMinY || y - r > viewMaxY) continue;

          const isFocused = activeFocusIdx === i;
          const isNeighbor = activeNeighbors.has(i);
          const isSearchMatch = hasSearch && n.label.toLowerCase().includes(q!);

          if (isFocused || isSearchMatch) {
            ctx.beginPath();
            ctx.arc(x, y, r + 6 / t.k, 0, 2 * Math.PI);
            ctx.fillStyle =
              isSearchMatch && !isFocused
                ? "rgba(255, 200, 100, 0.25)"
                : "rgba(100, 200, 255, 0.25)";
            ctx.fill();
          }

          ctx.beginPath();
          ctx.arc(x, y, r, 0, 2 * Math.PI);
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

          // Labels — only when LOD allows or the node is highlighted.
          const showLabel =
            !lodHideLabels &&
            (isFocused ||
              isNeighbor ||
              isSearchMatch ||
              (nodes.length < 100 && !hasFocus && !hasSearch));
          if (showLabel) {
            ctx.fillStyle =
              isFocused || isNeighbor || isSearchMatch
                ? "rgba(255, 255, 255, 1)"
                : "rgba(180, 180, 180, 0.8)";
            ctx.font = `${(isFocused ? 12 : 10) / t.k}px Inter, system-ui`;
            ctx.textAlign = "center";
            ctx.fillText(n.label, x, y - r - 5 / t.k);
          }
        }
      }

      // ── Activation halos ─────────────────────────────────────────
      const liveActivations = activationsRef.current;
      if (positions && Object.keys(liveActivations).length > 0) {
        const resolver = claimToEntitiesRef.current;
        const perNode = new Map<number, { intensity: number; kind: ActivationKind }>();
        for (const [claimId, activation] of Object.entries(liveActivations)) {
          const entityNames = resolver.get(claimId);
          if (!entityNames) continue;
          for (const name of entityNames) {
            const idx = idIndexRef.current.get(name);
            if (idx === undefined) continue;
            const prev = perNode.get(idx);
            if (!prev || activation.intensity > prev.intensity) {
              perNode.set(idx, activation);
            }
          }
        }
        if (perNode.size > 0) {
          ctx.save();
          for (const [idx, a] of perNode) {
            const node = nodes[idx];
            if (!node) continue;
            const x = positions[idx * 2];
            const y = positions[idx * 2 + 1];
            if (x === undefined || y === undefined) continue;
            const baseR = nodeRadius(node.claim_count);
            const haloR = baseR + 8 / t.k + a.intensity * 6;
            const { r: hr, g: hg, b: hb } = activationHue(a.kind);
            const opacity = Math.min(0.85, a.intensity);
            const grad = ctx.createRadialGradient(x, y, baseR, x, y, haloR);
            grad.addColorStop(0, `rgba(${hr}, ${hg}, ${hb}, ${opacity * 0.55})`);
            grad.addColorStop(1, `rgba(${hr}, ${hg}, ${hb}, 0)`);
            ctx.fillStyle = grad;
            ctx.beginPath();
            ctx.arc(x, y, haloR, 0, 2 * Math.PI);
            ctx.fill();
            ctx.beginPath();
            ctx.arc(x, y, baseR + 2 / t.k, 0, 2 * Math.PI);
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
    draw();

    return () => {
      drawRef.current = null;
    };
  }, [nodes, links, neighborMap, size]);

  // ── 5. Search-query ref mirror ──────────────────────────────────────
  useEffect(() => {
    searchQueryRef.current = searchQuery;
    drawRef.current?.();
  }, [searchQuery]);

  // ── 6. Activation store → ref + decay rAF ───────────────────────────
  useEffect(() => {
    const tick = () => {
      const now = performance.now();
      useBrainActivation.getState().decay(now);
      const live = useBrainActivation.getState().activations;
      activationsRef.current = Object.fromEntries(
        Object.entries(live).map(([id, a]) => [id, { intensity: a.intensity, kind: a.kind }]),
      );
      drawRef.current?.();
      if (Object.keys(live).length > 0 && isVisible) {
        decayRafRef.current = requestAnimationFrame(tick);
      } else {
        decayRafRef.current = null;
      }
    };

    const unsubscribe = useBrainActivation.subscribe((state) => {
      activationsRef.current = Object.fromEntries(
        Object.entries(state.activations).map(([id, a]) => [
          id,
          { intensity: a.intensity, kind: a.kind },
        ]),
      );
      drawRef.current?.();
      if (
        Object.keys(state.activations).length > 0 &&
        decayRafRef.current === null &&
        isVisible
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
  }, [isVisible]);

  // ── 7. Container resize observer ────────────────────────────────────
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

  // ── 8. Pan / zoom ───────────────────────────────────────────────────
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || zoomBehaviorRef.current) return;
    const z = zoom<HTMLCanvasElement, unknown>()
      .scaleExtent([0.02, 10])
      .on("zoom", (event) => {
        transformRef.current = event.transform;
        drawRef.current?.();
      });
    zoomBehaviorRef.current = z;
    select(canvas).call(z);
  }, [nodes.length]);

  // ── 9. Initial centre / restore transform ──────────────────────────
  //
  // When a persisted transform exists we use that — it preserves the
  // user's zoom and pan across reloads.  Otherwise we centre on the
  // origin with a scale chosen so the bounding box of `sqrt(N) * 30`
  // virtual units fits the viewport.
  const centeredRef = useRef(false);
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !zoomBehaviorRef.current) return;
    if (centeredRef.current || size.w === 0 || nodes.length === 0) return;

    const savedTransform = persistedHint?.transform;
    if (savedTransform) {
      const restored = zoomIdentity
        .translate(savedTransform.x, savedTransform.y)
        .scale(savedTransform.k);
      select(canvas).call(zoomBehaviorRef.current.transform, restored);
      transformRef.current = restored;
      centeredRef.current = true;
      return;
    }

    const initialScale = Math.min(1, 400 / Math.max(200, Math.sqrt(nodes.length) * 30));
    const initialTransform = zoomIdentity
      .translate(size.w / 2, size.h / 2)
      .scale(initialScale);
    select(canvas).call(zoomBehaviorRef.current.transform, initialTransform);
    transformRef.current = initialTransform;
    centeredRef.current = true;
  }, [size, nodes.length, persistedHint]);

  // ── 10. Hit detection (mouse → node) ───────────────────────────────
  const getHitNode = (e: React.MouseEvent<HTMLCanvasElement>): { idx: number; node: NodeMeta } | null => {
    const canvas = canvasRef.current;
    if (!canvas) return null;
    const positions = positionsRef.current;
    if (!positions) return null;

    const rect = canvas.getBoundingClientRect();
    const mouseX = e.clientX - rect.left;
    const mouseY = e.clientY - rect.top;
    const t = transformRef.current;
    const virtualX = (mouseX - t.x) / t.k;
    const virtualY = (mouseY - t.y) / t.k;

    // Front-to-back so the topmost-drawn node wins.
    for (let i = nodes.length - 1; i >= 0; i--) {
      const n = nodes[i];
      if (!n) continue;
      const x = positions[i * 2];
      const y = positions[i * 2 + 1];
      if (x === undefined || y === undefined) continue;
      const dx = x - virtualX;
      const dy = y - virtualY;
      const r = nodeRadius(n.claim_count);
      if (dx * dx + dy * dy < (r + 5 / t.k) ** 2) {
        return { idx: i, node: n };
      }
    }
    return null;
  };

  const handleMouseMove = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    setHovered(hit ? hit.node.id : null);
  };

  const handleClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const hit = getHitNode(e);
    if (!hit) {
      setIsolated(null);
      return;
    }
    setIsolated(hit.node.id);

    const canvas = canvasRef.current;
    const positions = positionsRef.current;
    if (!canvas || !zoomBehaviorRef.current || !positions) return;
    const x = positions[hit.idx * 2];
    const y = positions[hit.idx * 2 + 1];
    if (x === undefined || y === undefined) return;

    const scale = Math.max(1.5, transformRef.current.k);
    const targetX = size.w / 2 - x * scale;
    const targetY = size.h / 2 - y * scale;
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
      <GraphInteractionHint isPaused={!isVisible} />
    </div>
  );
}

// ───────────────────────── Auxiliaries ────────────────────────────────

/** Pan/zoom hint only — counts live in BrainView's top-right HUD. */
function GraphInteractionHint({ isPaused }: { isPaused: boolean }) {
  return (
    <div
      className={cn(
        "pointer-events-none absolute bottom-3 left-3 flex items-center gap-2",
        "rounded-lg border border-border/50 bg-surface/90 px-2.5 py-1",
        "text-[10px] text-muted-foreground shadow-sm backdrop-blur-md",
      )}
    >
      {isPaused ? (
        <>
          <span className="text-muted-foreground/80">paused</span>
          <span className="text-border/80">·</span>
        </>
      ) : null}
      <span>scroll to zoom · drag to pan</span>
    </div>
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
