# Contributing to ThinkingRoot Desktop

This directory was scaffolded by transplanting the desktop shell from
the [helloroot](https://github.com/DevbyNaveen/helloroot) project on
2026-04-27 (Step 8 of the OSS v0.1 implementation plan).

## License history

Helloroot was licensed under Apache-2.0; the relevant files were
relicensed to MIT for ThinkingRoot under a clean single-author
declaration:

- Helloroot author per `git log --format='%aN' | sort -u`:
  **DevbyNaveen** (single author).
- The same author owns the ThinkingRoot OSS engine workspace.
- No third-party contributions to helloroot exist that would require
  a separate Contributor License Agreement.

The pre-transplant history of the copied files lives in the helloroot
repo under Apache-2.0 and is not preserved in this repo. Files moved
here are governed by the workspace-level MIT grant in
`/Users/naveen/Desktop/thinkingroot/LICENSE` (or the public MIT
grant once published).

## What was kept vs dropped

| Origin (helloroot)                 | Outcome here                       |
|---|---|
| `src-tauri/src/main.rs`            | Verbatim (entrypoint trampoline).   |
| `src-tauri/src/lib.rs`             | Rewritten (drop chat/agents/capsules/covenant/trace invoke handlers). |
| `src-tauri/src/state.rs`           | Rewritten (drop helloroot orchestrator + capsule store + signing key). |
| `src-tauri/src/config.rs`          | Rewritten (rebrand path + env var). |
| `src-tauri/src/commands/meta.rs`   | Rewritten (drop helloroot crate VERSION refs). |
| `src-tauri/src/commands/memory.rs` | Rewritten (mount QueryEngine directly, drop ThinkingRootMemoryClient wrapper). |
| `src-tauri/src/commands/{fs,git,settings,workspaces}.rs` | Verbatim with env-var rebrand (`HELLOROOT_*` → `THINKINGROOT_*`). |
| `src-tauri/src/commands/{chat,agents,capsules,covenant,trace}.rs` | Dropped — out of scope here. |
| `src-tauri/src/agent_sink.rs`      | Dropped — sidecar (Step 10) replaces. |
| `src-tauri/tauri.conf.json`        | Verbatim with rebrand (HelloRoot → ThinkingRoot Desktop, `app.helloroot.desktop` → `dev.thinkingroot.desktop`, copyright). |
| `ui/`                              | Mirror in flight (Step 8 phase 2). |

## Adding new contributions

Open a PR against the OSS engine repo
(<https://github.com/DevbyNaveen/ThinkingRoot>). Contributions are
released under the workspace MIT license unless the PR explicitly
notes a different grant.
