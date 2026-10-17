#!/usr/bin/env bash
# Builds the Windows installer. On Windows this is the same as `npm run build`.
# From Linux/macOS it cross-compiles with cargo-xwin (NSIS installer only):
#   rustup target add x86_64-pc-windows-msvc
#   cargo install --locked cargo-xwin
#   sudo apt install nsis lld llvm clang     # Linux
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${OS:-}" == "Windows_NT" ]]; then
  npm run build -- "$@"
else
  npm run build -- --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis "$@"
fi
