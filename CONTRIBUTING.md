# Contributing to ThinkingRoot

Thank you for your interest in contributing. This guide covers everything you need to get started.

## Before You Start

- Check [open issues](https://github.com/thinkingroot/thinkingroot/issues) to avoid duplicate work
- For significant changes, open a Discussion first to align on direction
- Read this file fully — it covers the codebase architecture and where things live

## Setup

**Prerequisites:** Rust 1.85+

```bash
git clone https://github.com/thinkingroot/thinkingroot
cd thinkingroot
cargo build --no-default-features   # fast build, no ONNX Runtime
cargo test --no-default-features    # run tests
```

For the Python SDK, you also need Python 3.9+ and maturin:
```bash
pip install maturin
cd thinkingroot-python
maturin develop --release
```

## Development Workflow

1. Fork the repo and create a branch: `git checkout -b feat/my-feature`
2. Make your changes (see [Where to put code](#where-to-put-code) below)
3. Write or update tests
4. Run the checks:
   ```bash
   cargo fmt --all
   cargo clippy --workspace -- -D warnings
   cargo test --no-default-features
   ```
5. Commit with a clear message: `feat: add PDF parser for scanned documents`
6. Open a pull request using the provided template

## Where to Put Code

| What you're adding | File |
|---|---|
| New core type (Claim, Entity variant, etc.) | `crates/thinkingroot-core/src/types/` |
| New file parser | `crates/thinkingroot-parse/src/` |
| New LLM extraction prompt | `crates/thinkingroot-extract/src/prompts.rs` |
| New graph relation / schema | `crates/thinkingroot-graph/src/graph.rs` |
| New artifact type | `crates/thinkingroot-compile/src/compiler.rs` |
| New REST endpoint | `crates/thinkingroot-serve/src/rest.rs` |
| New MCP tool | `crates/thinkingroot-serve/src/mcp/tools.rs` |
| New CLI command | `crates/thinkingroot-cli/src/main.rs` |
| New Python binding | `thinkingroot-python/src/lib.rs` |
| New verification check | `crates/thinkingroot-verify/src/verifier.rs` |

## Code Style

- Follow `rustfmt` defaults (enforced by CI)
- Keep functions focused — prefer many small functions over one large one
- No unnecessary abstractions — if something is used once, inline it
- Error types live in `thinkingroot-core::Error` — add variants there, not per-crate
- Use `tracing::info!()` / `tracing::warn!()` for user-visible messages, `tracing::debug!()` for internals
- All async code uses tokio

## Feature Flags

The `vector` feature controls fastembed + ONNX Runtime linkage (~300MB). It is **on by default** but must be opt-in in code:

```toml
# In your crate's Cargo.toml — always use [features], never explicit dep features
[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector"]
```

Tests that don't test vector search should pass with `--no-default-features`.  
CI always runs with `--no-default-features` to keep builds fast.

## Testing

- Unit tests live alongside the code in `#[cfg(test)] mod tests { ... }`
- Integration tests live in `crates/<name>/tests/`
- Use `tempfile` for any test that needs the filesystem
- LLM calls must be mocked in tests — use the existing mock patterns in `thinkingroot-extract`
- Python tests: `cd thinkingroot-python && maturin develop && pytest`

## Commit Messages

Use conventional commits:
- `feat: add YAML parser`
- `fix: handle empty source gracefully`
- `docs: clarify MCP SSE flow`
- `test: add contradiction detection edge case`
- `refactor: extract cosine similarity to shared util`
- `chore: update fastembed to 5.1`

## What We Won't Merge

- Changes that break `cargo test --no-default-features`
- Adding `features = ["vector"]` to a dependency declaration (breaks `--no-default-features` globally)
- Committing `.thinkingroot/`, `.env`, or API keys
- Removing the `default-members` exclusion of `thinkingroot-python`
- Adding `unwrap()` in non-test code without a comment explaining why it's safe

## Getting Help

- [GitHub Discussions](https://github.com/thinkingroot/thinkingroot/discussions) for questions
- Open an issue for bugs using the template
- Open a [Discussion](https://github.com/thinkingroot/thinkingroot/discussions) for architectural questions

## License

By contributing, you agree that your contributions will be licensed under the same MIT OR Apache-2.0 terms as the project.
