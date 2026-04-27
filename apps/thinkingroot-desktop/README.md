# ThinkingRoot Desktop

Tauri 2 desktop app for the ThinkingRoot OSS engine. Local-first
knowledge compiler with an in-app brain view, workspace manager, and
on-device privacy dashboard.

## Status

Phase F Stream H — scaffolded 2026-04-27 (Step 8 of the OSS v0.1
implementation plan). The Rust backend embeds
`thinkingroot-serve::engine::QueryEngine` for in-process workspace
queries; chat / agent orchestration is delegated to an
out-of-process agent-runtime sidecar (Step 10).

## Layout

```
apps/thinkingroot-desktop/
├── src-tauri/        # Rust + Tauri 2 backend (this dir)
│   ├── src/
│   │   ├── commands/ # Tauri IPC handlers
│   │   ├── config.rs # Desktop config (~/.config/thinkingroot/desktop.toml)
│   │   ├── state.rs  # AppState (lazily-mounted QueryEngine)
│   │   └── lib.rs    # Plugin wiring + invoke_handler
│   ├── Cargo.toml    # Stand-alone workspace
│   └── tauri.conf.json
└── ui/               # React + Vite frontend (Step 8 phase 2)
    └── …
```

## Develop

```bash
cd apps/thinkingroot-desktop
pnpm --dir ui install          # one-time
pnpm tauri dev                  # opens 1280×800 window on localhost:1420
```

## Build

```bash
cd apps/thinkingroot-desktop
pnpm tauri build                # produces signed-or-unsigned bundles
```

## Honest gap

Step 8 ships the Rust scaffold + verbatim helloroot-derived modules.
The frontend mirror (~4,500 LOC of TSX) is in flight as a follow-up
batch within this same step. Once both halves land, `pnpm tauri dev`
boots cleanly — until then the backend builds in isolation via
`cargo check`.

## Origin

The Rust + UI shells in this directory were derived from the
[helloroot](https://github.com/DevbyNaveen/helloroot) project and
relicensed from Apache-2.0 to MIT. Single-author origin
([@DevbyNaveen](https://github.com/DevbyNaveen)) — see
`CONTRIBUTING.md` for the relicense paper-trail.

## License

MIT — see `../LICENSE` for the workspace-level grant.
