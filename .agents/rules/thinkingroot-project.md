# ThinkingRoot Project Rule

## Product Framing

ThinkingRoot is infrastructure for AI agents that need grounded, verifiable memory and knowledge retrieval. Keep product language centered on deterministic compile, byte-grounded Witnesses, content-addressed packs, and fast query surfaces.

## Engineering Invariants

- Compile is mechanical: tree-sitter, LSP, doctags, regex, rule catalog, Witness Mesh.
- The agent's own LLM talks at query time; compile does not use an LLM.
- Witness IDs are deterministic and byte-grounded.
- Empty or missing data should surface honestly.
- Cross-repo dependencies with `thinkingroot-cloud` must stay compatible.
- `root serve` defaults to REST/MCP daemon behavior; MCP stdio is available via `root serve --mcp-stdio`.

## Local Workflow

- Use `cargo test --workspace` as the broad baseline.
- Use `cargo check --workspace` when Python bindings or excluded workspace members matter.
- Use targeted crate tests for narrow edits.
- Avoid `cargo clean` during active debugging.
- Do not modify unrelated local changes.

## Repository Memory

Before non-trivial work, read `CLAUDE.md`. For code covered by `.claude/rules/*.md`, read the relevant rule file before editing.
