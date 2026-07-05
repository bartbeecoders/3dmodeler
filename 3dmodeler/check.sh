#!/usr/bin/env bash
# CI-style check: everything must compile for native and wasm, tests must pass.
set -euo pipefail
cd "$(dirname "$0")"

cargo test --workspace
cargo check --workspace
cargo check --workspace --target wasm32-unknown-unknown
echo "✅ all checks passed"
