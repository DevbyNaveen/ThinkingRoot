/// <reference lib="webworker" />
/**
 * Brain-graph force-layout worker.
 *
 * Holds the d3-force simulation off the main thread.  Each tick the
 * worker packs the `(x, y)` of every node into a Float32Array and
 * posts it back as a transferable, so the main thread does only a
 * pointer-flip per frame rather than serialising a 9k-element array.
 *
 * Protocol (main → worker):
 *
 *   ```
 *   { type: "init", nodes, links, alpha?, alphaMin?, alphaDecay? }
 *   { type: "reheat", alpha? }
 *   { type: "stop" }
 *   { type: "pause" }
 *   { type: "resume" }
 *   { type: "update", nodes, links, alpha? }   // incremental refresh
 *   ```
 *
 * Protocol (worker → main):
 *
 *   ```
 *   { type: "tick", positions: Float32Array, alpha: number, ids: string[]? }
 *   { type: "end", positions: Float32Array, ids: string[] }
 *   ```
 *
 * The `ids` array is sent once on the first tick after `init` /
 * `update` so the main thread can map index → entity name; later
 * ticks omit it since the order is stable until the next `update`.
 *
 * The simulation's natural lifecycle drives the message stream:
 * d3-force ticks at ~60 Hz internally while `alpha > alphaMin`, then
 * fires `on("end")` exactly once.  We don't impose a setInterval —
 * d3's timer does the right thing.
 *
 * `pause` calls `simulation.stop()`; `resume` calls `restart()` at
 * the current alpha so the user's view-time costs nothing while the
 * Brain tab is hidden.
 */

import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";

interface NodeIn {
  id: string;
  claim_count: number;
  /** Optional warm-start position from the persisted layout cache. */
  x?: number;
  y?: number;
}

interface LinkIn {
  source: string;
  target: string;
  strength?: number;
}

interface InitMsg {
  type: "init";
  nodes: NodeIn[];
  links: LinkIn[];
  alpha?: number;
  alphaMin?: number;
  alphaDecay?: number;
}

interface UpdateMsg {
  type: "update";
  nodes: NodeIn[];
  links: LinkIn[];
  alpha?: number;
}

interface ReheatMsg {
  type: "reheat";
  alpha?: number;
}

type InMsg =
  | InitMsg
  | UpdateMsg
  | ReheatMsg
  | { type: "stop" }
  | { type: "pause" }
  | { type: "resume" };

interface SimNode extends SimulationNodeDatum {
  id: string;
  claim_count: number;
}

interface SimLink extends SimulationLinkDatum<SimNode> {
  strength?: number;
}

let sim: Simulation<SimNode, SimLink> | null = null;
let nodes: SimNode[] = [];
let idsDirty = true;

function buildSimulation(initial: InitMsg | UpdateMsg): void {
  // Stop the previous instance fully — `.stop()` cancels the timer
  // but doesn't release listeners until we drop the reference.
  if (sim) {
    sim.on("tick", null);
    sim.on("end", null);
    sim.stop();
  }

  const simNodes: SimNode[] = initial.nodes.map((n) => ({
    id: n.id,
    claim_count: n.claim_count,
    x: n.x,
    y: n.y,
  }));
  const simLinks: SimLink[] = initial.links.map((l) => ({
    source: l.source,
    target: l.target,
    strength: l.strength,
  }));

  // Resolve link endpoints by id via the linkForce; this lets d3
  // operate on string ids in the input without us pre-resolving to
  // node references.
  sim = forceSimulation<SimNode>(simNodes)
    .alphaDecay(initial.type === "init" ? (initial.alphaDecay ?? 0.015) : 0.025)
    .alphaMin(initial.type === "init" ? (initial.alphaMin ?? 0.001) : 0.001)
    .force(
      "link",
      forceLink<SimNode, SimLink>(simLinks)
        .id((d) => d.id)
        .distance(60)
        .strength(0.5),
    )
    .force("charge", forceManyBody<SimNode>().strength(-150))
    .force("center", forceCenter(0, 0))
    .force(
      "collide",
      forceCollide<SimNode>()
        .radius((d) => 4 + Math.sqrt(d.claim_count) * 1.5)
        .iterations(2),
    );

  nodes = simNodes;
  idsDirty = true;

  sim.on("tick", () => emitTick(false));
  sim.on("end", () => emitTick(true));

  const startingAlpha = initial.alpha ?? 1;
  sim.alpha(startingAlpha).restart();
}

function emitTick(isEnd: boolean): void {
  if (!sim) return;
  const n = nodes.length;
  const positions = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) {
    const node = nodes[i];
    if (!node) continue;
    positions[i * 2] = node.x ?? 0;
    positions[i * 2 + 1] = node.y ?? 0;
  }

  // `ids` is the index→name mapping.  We send it once after each
  // (re)build so the main thread can rebind labels; subsequent ticks
  // are pure number streams and travel as transferables.
  if (idsDirty) {
    idsDirty = false;
    const ids = nodes.map((nd) => nd.id);
    (self as DedicatedWorkerGlobalScope).postMessage(
      { type: isEnd ? "end" : "tick", positions, alpha: sim.alpha(), ids },
      [positions.buffer],
    );
  } else {
    (self as DedicatedWorkerGlobalScope).postMessage(
      { type: isEnd ? "end" : "tick", positions, alpha: sim.alpha() },
      [positions.buffer],
    );
  }
}

self.onmessage = (e: MessageEvent<InMsg>) => {
  const msg = e.data;
  switch (msg.type) {
    case "init":
      buildSimulation(msg);
      break;

    case "update": {
      // Preserve positions for entities that survived from the old
      // graph; new ones inherit `x: undefined` and d3 starts them
      // near the center.  This is the diff-refresh path that keeps
      // the layout coherent across compile-done events.
      if (sim) {
        const prev = new Map<string, { x: number; y: number }>();
        for (const node of nodes) {
          if (node.x != null && node.y != null) {
            prev.set(node.id, { x: node.x, y: node.y });
          }
        }
        for (const n of msg.nodes) {
          if (n.x == null || n.y == null) {
            const carried = prev.get(n.id);
            if (carried) {
              n.x = carried.x;
              n.y = carried.y;
            }
          }
        }
      }
      buildSimulation(msg);
      break;
    }

    case "reheat":
      if (sim) {
        sim.alpha(msg.alpha ?? 0.3).restart();
      }
      break;

    case "pause":
      if (sim) sim.stop();
      break;

    case "resume":
      if (sim) {
        // Gentle warm-up — the graph is usually already laid out, we
        // just need a couple of frames to absorb any sub-frame drift
        // and let the user-visible canvas catch up.
        sim.alpha(Math.max(sim.alpha(), 0.05)).restart();
      }
      break;

    case "stop":
      if (sim) {
        sim.on("tick", null);
        sim.on("end", null);
        sim.stop();
      }
      sim = null;
      nodes = [];
      break;
  }
};
