# 2026-04-30 — v1 wire format removal: cross-repo coordination

## What landed in OSS

Commits land on `thinkingroot/main` removing the entire v1 wire format
surface from `tr-format` and the v1 trust verifier from `tr-verify`.

| Symbol | Status |
|---|---|
| `tr_format::Manifest` (v1) | **deleted** |
| `tr_format::TrustTier` | **deleted** |
| `tr_format::writer::PackBuilder` | **deleted** |
| `tr_format::reader::Pack`, `read_bytes`, `read_file` | **deleted** |
| `tr_verify::Verifier`, `VerifierConfig` | **deleted** |
| `tr_verify::Verdict`, `RevokedDetails`, `TamperedKind`, `VerifiedDetails` | **deleted** |
| `tr_verify::AuthorKeyStore`, `TrustedAuthorKey` | **deleted** |
| `tr-c2pa` crate (deferred to v3.2+ per v3 spec §11) | **deleted** |
| `Capabilities` v1 wire (`tr_format::capabilities`) | **deleted** |

What remains in `tr_format`: `ManifestV3`, `V3PackBuilder`, `read_v3_pack`,
`V3Pack`, `ClaimRecord` — i.e., only the v3 wire format.

What remains in `tr_verify`: `verify_v3_pack`, `verify_v3_pack_with_revocation`,
`V3Verdict`, `V3TamperedKind` — i.e., only the v3 trust path.

## What the cloud sibling needs to do

`thinkingroot-cloud/services/registry` currently has these v1 dependencies
(verified by grep on `~/Desktop/thinkingroot-cloud/` at OSS commit point):

```
services/registry/src/service.rs:17   use tr_format::{digest::blake3_hex, reader};
services/registry/src/service.rs:356   use tr_format::Error as FE;
services/registry/tests/integration.rs:21
                                use tr_format::{writer::PackBuilder, Manifest, TrustTier};
```

The next `tr-format` workspace bump will fail to compile in cloud-side
CI until cloud is updated. Coordinated changes the cloud team must
land:

1. **`services/registry/src/service.rs`** — replace `tr_format::reader::read_bytes` with `tr_format::read_v3_pack`. The new return type is `V3Pack { manifest: ManifestV3, source_bundle: Vec<u8>, claims_jsonl: Vec<u8>, signature: Option<SigstoreBundle> }` instead of the v1 `reader::Pack`. Adapt registry storage / serving code accordingly. `tr_format::Error` is unchanged.

2. **`services/registry/tests/integration.rs`** — rewrite the test fixtures to build v3 packs via `V3PackBuilder` instead of `PackBuilder`. The v3 builder takes a `ManifestV3` (no `TrustTier`; signing is opt-in via `build_signed`/`build_with_signer`). Existing `manifest.toml` fixtures need regeneration as v3 manifests.

3. **Storage layout** — if registry CDN/blob store keys are derived from manifest fields (e.g., `<owner>/<slug>/<version>.tr`), v3 manifests carry the same `name` (`<owner>/<slug>`) and `version` (semver). No URL scheme break.

4. **Bump `Cargo.lock`** in cloud after the OSS-side changes are pulled into the path-dep target.

5. **Backfill any v1 packs** previously published to the cloud registry. v3.0-rc registry: empty / dev-only, no production v1 packs to migrate. If your environment has v1 packs that need to remain installable, mirror them as v3 by re-extracting source + claims and re-packing via `V3PackBuilder`.

## Why the OSS side cleaned first

- OSS workspace version is pre-v1.0 (`0.9.1`); breaking changes in
  pre-1.0 crates are expected per SemVer.
- The cloud sibling repo is a separate path-dep with its own commit
  cadence; coupling its release schedule to every OSS change would
  block both teams unnecessarily.
- The `MEMORY.md` baseline (from `MEMORY.md` in OSS) records the cloud
  sibling as a known consumer. The OSS-side breaking change is signalled
  via this doc + `CHANGELOG.md` + the next workspace version bump.

## Verification before merging the cloud side

```bash
# In cloud sibling repo, with OSS path-dep updated:
cd ~/Desktop/thinkingroot-cloud
cargo check --workspace
cargo test --workspace -p registry
```

If both pass, merge cloud's paired commit.
