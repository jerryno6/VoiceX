#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script only runs on macOS." >&2
  exit 1
fi

IDENTITY_NAME="${VOICEX_LOCAL_SIGN_IDENTITY:-VoiceX Local Code Signing}"
APP_NAME="${VOICEX_APP_NAME:-VoiceX.app}"

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
  echo "Ensuring both Rust targets are installed..."
  rustup target add x86_64-apple-darwin aarch64-apple-darwin
  TAURI_ARGS+=(--target universal-apple-darwin)
  SOURCE_APP="${PWD}/src-tauri/target/universal-apple-darwin/release/bundle/macos/${APP_NAME}"
else
  SOURCE_APP="${PWD}/src-tauri/target/release/bundle/macos/${APP_NAME}"
fi

TARGET_APP="/Applications/${APP_NAME}"

if ! security find-identity -v -p codesigning | grep -Fq "${IDENTITY_NAME}"; then
  echo "Missing signing identity: ${IDENTITY_NAME}" >&2
  echo "Run: pnpm mac:setup-signing" >&2
  exit 1
fi

echo "Building signed macOS release with identity: ${IDENTITY_NAME}"
if [[ $UNIVERSAL -eq 1 ]]; then
  echo "Building universal binary (x86_64 + arm64)..."
fi
pnpm tauri build "${TAURI_ARGS[@]}"

if [[ ! -d "${SOURCE_APP}" ]]; then
  echo "Bundle not found: ${SOURCE_APP}" >&2
  exit 1
fi

echo "Verifying app signature..."
codesign --verify --deep --strict --verbose=2 "${SOURCE_APP}"

echo "Installing to ${TARGET_APP}..."
rm -rf "${TARGET_APP}"
cp -R "${SOURCE_APP}" "${TARGET_APP}"
xattr -dr com.apple.quarantine "${TARGET_APP}" || true

echo "Installed. Signing summary:"
codesign -dvv "${TARGET_APP}" 2>&1 | grep -E "Identifier=|TeamIdentifier=|Authority=|Signature=|Executable="
