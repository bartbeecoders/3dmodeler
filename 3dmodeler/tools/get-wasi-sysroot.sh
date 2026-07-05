#!/usr/bin/env bash
# Downloads the wasi sysroot used to cross-compile box3d (C17) to wasm32.
set -euo pipefail
cd "$(dirname "$0")"

VERSION=33
if [ -d wasi-sysroot ]; then
    echo "wasi-sysroot already present"
    exit 0
fi

curl -sL -o wasi-sysroot.tar.gz \
    "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${VERSION}/wasi-sysroot-${VERSION}.0%2Bm.tar.gz"
tar xzf wasi-sysroot.tar.gz
rm wasi-sysroot.tar.gz
mv wasi-sysroot-* wasi-sysroot
echo "wasi-sysroot installed"
