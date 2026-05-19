# Unified macOS App — Universal Binary Plan

## Context

VoiceX currently builds **single-architecture** macOS bundles. The build script `scripts/build-macos-local-release.sh` runs `pnpm tauri build` natively, producing only the host machine's arch (`aarch64` on Apple Silicon, `x86_64` on Intel). `RELEASE.md` anticipates two separate DMGs (`VoiceX_X.Y.Z_aarch64.dmg`). The goal: produce one **universal** `.app` / `.dmg` that installs and runs on both Intel and Apple Silicon Macs from a single download.

Source audit (May 2026) confirmed the codebase is largely arch-agnostic:

- No `#[cfg(target_arch …)]` or hardcoded arch strings in Rust source under `src-tauri/src/`.
- All macOS native crates (`cpal`, `objc2`, `enigo`, vendored `rdev`, vendored `audiopus_sys`) compile cleanly for both `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- Frontend is pure JS — Vite emits arch-neutral output. Both `@esbuild/darwin-arm64` and `@esbuild/darwin-x64` (plus rollup equivalents) are already pinned in `pnpm-lock.yaml`.
- `minimumSystemVersion: 10.15` in `src-tauri/tauri.conf.json` covers Intel Macs back to 2013 hardware and all Apple Silicon machines.

Only the **build pipeline** needs work. Tauri 2 supports universal builds natively via `cargo tauri build --target universal-apple-darwin`, which compiles both Rust targets and `lipo`s them into a fat Mach-O — provided both Rust targets are installed and the CMake build of vendored Opus produces correct per-arch objects.

---

## Outcome

- `pnpm mac:build-universal` produces `VoiceX.app` whose main Mach-O contains both `x86_64` and `arm64` slices (`lipo -info` confirms).
- Resulting `.dmg` (e.g. `VoiceX_<version>_universal.dmg`) installs and launches on both architectures.
- No Rust source changes; only build scripts, `package.json`, optionally `.cargo/config.toml`, optionally CI.
- Existing per-arch local-build flow (`pnpm mac:build-local`) still works for fast iteration.

---

## Approach

Use Tauri's first-class universal target. The change set is small:

1. Install both Rust targets on the build machine: `rustup target add x86_64-apple-darwin aarch64-apple-darwin`.
2. Invoke `pnpm tauri build --target universal-apple-darwin`. Tauri builds twice (once per arch) using isolated `target/<triple>/` directories, then `lipo`s the final binary and produces a single `.app` bundle.
3. Sign the universal bundle once. `codesign` handles fat Mach-O natively.

The vendored `audiopus_sys` build uses the `cmake` crate, which reads `CARGO_CFG_TARGET_ARCH` and sets `CMAKE_OSX_ARCHITECTURES` automatically per Rust target. Each pass compiles Opus for the correct slice in its own `target/<triple>/` directory — no build.rs change required.

---

## Files to Modify

| Path | Change |
|------|--------|
| `scripts/build-macos-local-release.sh` | Accept a `--universal` flag. When set: ensure both Rust targets are installed, pass `--target universal-apple-darwin` to `tauri build`, and point `SOURCE_APP` at `src-tauri/target/universal-apple-darwin/release/bundle/macos/${APP_NAME}`. |
| `package.json` | Add script `"mac:build-universal": "bash ./scripts/build-macos-local-release.sh --universal"`. |
| `.cargo/config.toml` | No change expected. Only edit if linker flags are needed for one of the cross targets. |
| `src-tauri/tauri.conf.json` | No change. `minimumSystemVersion: "10.15"` and `targets: "all"` are already correct. |
| `.github/workflows/macos-release.yml` | **New** (optional, deferrable). Runs on `macos-14`, installs both Rust targets, builds universal, uploads `.dmg` + `.app.tar.gz` artifact. |
| `RELEASE.md` | Update macOS section to reference a single `VoiceX_<v>_universal.dmg` instead of separate per-arch DMGs. |
| `plans/unified-mac-app.md` | This document. |

No changes to: `src-tauri/src/**`, `src-tauri/Cargo.toml`, `src-tauri/Info.plist`, `src-tauri/build.rs`, vendored `audiopus_sys` / `rdev`, or any frontend code.

---

## Detailed Steps

### Step 1 — Confirm vendored audiopus_sys handles universal cleanly

`src-tauri/vendor/audiopus_sys/build.rs` uses the `cmake` crate. The crate respects `CARGO_CFG_TARGET_ARCH` and sets `CMAKE_OSX_ARCHITECTURES` per-target when invoked under cargo with `--target <triple>`. Since Tauri's universal build runs cargo twice with different `--target` values in isolated `target/<triple>/` directories, each CMake invocation produces an Opus static library for the correct arch.

No code change. Verification step in `## Verification` below.

### Step 2 — Extend the local build script

Edit `scripts/build-macos-local-release.sh` to parse a `--universal` flag (sketch):

```bash
UNIVERSAL=0
TAURI_ARGS=()
for arg in "$@"; do
  if [[ "$arg" == "--universal" ]]; then
    UNIVERSAL=1
  else
    TAURI_ARGS+=("$arg")
  fi
done

if [[ $UNIVERSAL -eq 1 ]]; then
  rustup target add x86_64-apple-darwin aarch64-apple-darwin
  TAURI_ARGS+=(--target universal-apple-darwin)
  SOURCE_APP="${PWD}/src-tauri/target/universal-apple-darwin/release/bundle/macos/${APP_NAME}"
fi

pnpm tauri build "${TAURI_ARGS[@]}"
```

Add the npm script:

```json
"mac:build-universal": "bash ./scripts/build-macos-local-release.sh --universal"
```

### Step 3 — Sign and install

The existing flow (`codesign --verify --deep --strict`, copy to `/Applications/`, strip quarantine) is unchanged. The local-signing identity (`VoiceX Local Code Signing` by default) signs the fat Mach-O as a single unit.

### Step 4 — Optional CI workflow

Add `.github/workflows/macos-release.yml`:

- Trigger: tag push matching `v*` or manual `workflow_dispatch`.
- Runner: `macos-14` (Apple Silicon GitHub-hosted runner; ships with both arch toolchains via Xcode).
- Steps:
  1. `actions/checkout@v4`
  2. Setup pnpm + Node.
  3. `rustup target add x86_64-apple-darwin aarch64-apple-darwin`
  4. `pnpm install --frozen-lockfile`
  5. `pnpm tauri build --target universal-apple-darwin`
  6. Upload `.dmg` + `.app.tar.gz` as workflow artifacts (or attach to a draft release).

CI does **not** code-sign in v1 — matches the current local-only signing posture. Distribution signing + notarization is a separate follow-up.

### Step 5 — Update RELEASE.md

Replace the two per-arch DMG references with one `VoiceX_<v>_universal.dmg`. Add a short note that the universal DMG supersedes the prior split releases.

---

## Verification

1. **Build succeeds**: `pnpm mac:build-universal` exits 0 on an Apple Silicon dev machine.
2. **Fat binary check**:

   ```bash
   lipo -info src-tauri/target/universal-apple-darwin/release/bundle/macos/VoiceX.app/Contents/MacOS/VoiceX
   # Expected: Architectures in the fat file: ... x86_64 arm64
   ```

3. **Signature**: `codesign --verify --deep --strict --verbose=2 …/VoiceX.app` passes.
4. **Sanity launch on host arch**: app opens, mic permission prompt fires, end-to-end recording (hotkey → ASR → paste) succeeds.
5. **Cross-arch run**: copy the `.app` to an Intel Mac and launch. If no Intel hardware is available, force the other slice on an Apple Silicon machine:

   ```bash
   arch -x86_64 /Applications/VoiceX.app/Contents/MacOS/VoiceX
   ```

   It should start and capture audio (Rosetta 2 must be installed for the x86_64 slice).
6. **Pre-flight (per project CLAUDE.md)**:

   ```bash
   pnpm build
   (cd src-tauri && cargo check --target aarch64-apple-darwin)
   (cd src-tauri && cargo check --target x86_64-apple-darwin)
   ```

   All three should succeed without warnings introduced by this change.

---

## Risks and Open Questions

- **Build time doubles.** Two arches compile sequentially. Acceptable for release builds; local iteration keeps the native-only `pnpm mac:build-local` path.
- **CMake cache leakage.** Tauri isolates per-target builds under `target/<triple>/`, so cross-contamination is unlikely. If a stale `target/release/` from a prior single-arch build interferes, run `cargo clean` before the first universal build.
- **`audiopus_sys` static linking** is already enforced on macOS (`vendor/audiopus_sys/build.rs:84`). No dylib rewriting required.
- **Notarization is out of scope.** Current local builds use ad-hoc signing. Notarized Developer ID signing for public distribution is a follow-up.
- **macOS-14 GitHub runner cost** is roughly 10× linux. If CI is too expensive, keep the local script as the canonical release path and skip the workflow.

---

## Out of Scope

- Apple Developer ID signing and notarization automation.
- Windows ARM64 build.
- Slimming the universal binary via `lipo -extract` for arch-specific distribution channels.
- Pruning architectures from bundled frameworks (Tauri/WebKit handle this themselves).
