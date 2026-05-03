# thinkingroot — TypeScript SDK

The TypeScript / Node SDK for [ThinkingRoot](https://thinkingroot.dev) —
the secondary brain for AI agents.

## Install

```bash
npm install thinkingroot
```

Requires Node 18+ (uses the built-in `fetch`).

## Quick start

```ts
import { Brain } from "thinkingroot";

// Cortex-aware: discovers a running `root serve` daemon via the
// shared lockfile.
const brain = await Brain.connect();

// Build an Engram for a topic, then probe it with questions.
const { pointer } = await brain.materializeEngram("auth flow");
const answer = await brain.probe(pointer, "what changed last week?");

answer.answer.forEach((row, i) => {
  console.log(answer.claim_ids[i], row);
});

// Or run hybrid retrieval directly.
const hits = await brain.hybridSearch("session timeout", { top_k: 10 });
hits.hits.forEach((h) => console.log(h.claim_id, h.score));
```

## Three transports

```ts
// Explicit URL.
const brain = await Brain.remote("http://127.0.0.1:31760");

// Cortex-aware auto-discovery (recommended).
const brain = await Brain.connect();

// Spawn `root mount <pack.tr>` and attach to the result.
const brain = await Brain.mount("./shared-knowledge.tr");
```

All three return the same `Brain` shape — swap transports without
rewriting code.

## Surfaces

| Method | REST endpoint |
|---|---|
| `entities()` | `GET /api/v1/ws/{ws}/entities` |
| `entity(name)` | `GET /api/v1/ws/{ws}/entities/{name}` |
| `claims({type, minConfidence, limit})` | `GET /api/v1/ws/{ws}/claims` |
| `search(query, topK)` | `GET /api/v1/ws/{ws}/search` |
| `hybridSearch(query, opts)` | `POST /api/v1/ws/{ws}/search/hybrid` |
| `materializeEngram(topic, opts)` | `POST /api/v1/ws/{ws}/engrams` |
| `probe(pointer, question, opts)` | `POST /api/v1/ws/{ws}/engrams/{ptr}/probe` |
| `engrams()` | `GET /api/v1/ws/{ws}/engrams` |
| `expire(pointer)` | `DELETE /api/v1/ws/{ws}/engrams/{ptr}` |
| `resetSession()` | (loops over the above) |

## Cortex-only API

For lower-level cortex.lock discovery without the Brain facade:

```ts
import { readLock, processAlive, healthCheck } from "thinkingroot/cortex";

const lock = await readLock();
if (lock && processAlive(lock.pid) && (await healthCheck(lock.host, lock.port))) {
  console.log(`daemon running at ${lock.host}:${lock.port} (pid=${lock.pid})`);
}
```

## License

MIT.
