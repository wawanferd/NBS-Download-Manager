#!/usr/bin/env bash
# Builds the Windows installer (NSIS .exe). Works on Windows and, via
# cargo-xwin, cross-compiles from Linux/macOS:
#   rustup target add x86_64-pc-windows-msvc
#   cargo install --locked cargo-xwin
#   sudo apt install nsis lld llvm clang     # Linux
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-msvc
if [[ "${OS:-}" == "Windows_NT" ]]; then
  CARGO=(cargo build)
  RUNNER=()
else
  CARGO=(cargo xwin build)
  RUNNER=(--runner cargo-xwin)
fi

# The native messaging host is shipped next to the app (bundle.externalBin).
"${CARGO[@]}" --release -p nbs-native-host --target "$TARGET"
mkdir -p src-tauri/binaries
cp "target/$TARGET/release/nbs-native-host.exe" "src-tauri/binaries/nbs-native-host-$TARGET.exe"

# MSI (WiX) can only be built on Windows; NSIS works everywhere.
BUNDLES="${BUNDLES:-nsis}"
npx tauri build "${RUNNER[@]}" --target "$TARGET" --bundles "$BUNDLES" "$@"
echo
ls -1 target/$TARGET/release/bundle/*/*.{exe,msi} 2>/dev/null || true
