# thinkingroot — Python SDK

The Python SDK for [ThinkingRoot](https://thinkingroot.dev) — the
secondary brain for AI agents.

## Install

```bash
pip install thinkingroot
```

The package ships native PyO3 bindings for in-process use plus an
`httpx`-backed remote client.

## Quick start

```python
from thinkingroot import Brain

# Cortex-aware: discovers a running `root serve` daemon.
brain = Brain.connect()

# Materialize an Engram and probe it.
result = brain.materialize_engram("auth flow")
answer = brain.probe(result["pointer"], "what changed last week?")

for claim_id, row in zip(answer["claim_ids"], answer["answer"]):
    print(claim_id, row)

# Hybrid retrieval (vector x Datalog x BLAKE3 x 11-component score).
hits = brain.hybrid_search("session timeout", top_k=10)
for h in hits["hits"]:
    print(h["claim_id"], h["score"], h["score_breakdown"])
```

## Three transports — same surface

```python
from thinkingroot import Brain

# In-process (PyO3).  Sub-millisecond queries, no network.
brain = Brain.open("/path/to/compiled-workspace")

# Explicit remote URL.
brain = Brain.remote("http://127.0.0.1:31760")

# Cortex-aware auto-discovery (recommended).
brain = Brain.connect()

# Spawn `root mount <pack.tr>` and attach.
brain = Brain.mount("./shared-knowledge.tr")
```

All four return a `Brain` with identical methods — swap transports
freely.

## Method surface

| Method | What |
|---|---|
| `entities()` | List entities in the workspace |
| `entity(name)` | Get a single entity |
| `claims(claim_type=, min_confidence=)` | List claims (optionally filtered) |
| `relations(entity)` | Outgoing relations for an entity |
| `search(query, top_k=10)` | Keyword + vector search |
| `hybrid_search(query, ...)` | 11-component hybrid retrieval |
| `materialize_engram(topic, ...)` | Build an Engram → returns `{pointer, summary}` |
| `probe(pointer, question, ...)` | Probe an Engram → returns `ProbeAnswer` |
| `engrams()` | List active engrams in this Brain's session |
| `expire(pointer)` | Drop an engram |
| `reset_session()` | Drop every engram in the session |

## Cortex-only API

```python
from thinkingroot.cortex import read_lock, process_alive

lock = read_lock()
if lock and process_alive(lock.pid):
    print(f"daemon at {lock.host}:{lock.port} (pid {lock.pid})")
```

## License

MIT.
