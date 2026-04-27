# App icons

Tauri expects the following files for bundled releases:

- `32x32.png`
- `128x128.png`
- `128x128@2x.png`
- `icon.icns` (macOS)
- `icon.ico` (Windows)

Generate with:

```sh
pnpm tauri icon ./path/to/source-logo.png
```

For D-1 (scaffold) we run `tauri dev`, which uses the Tauri default
icon — no files required. Add real icons before D-11 (signed release).
