#!/usr/bin/env bash

# Builds the Box3D examples: the "samples" app bundles every sample_*.cpp
# scene into one GUI executable, selectable from an in-app list at runtime.
# Run this from a bash shell; results land in build/bin/samples.
set -euo pipefail
cd "$(dirname "$0")"

# An active Conda (or similar) environment can inject its own CC/CXX,
# CMAKE_PREFIX_PATH, and CMAKE_ARGS, shadowing the system GTK3/fontconfig
# that the native file dialog (nfd) needs on Linux. Force the system
# toolchain so configure/build see the right compiler and pkg-config paths.
unset CC CXX CFLAGS CXXFLAGS CPPFLAGS LDFLAGS CMAKE_PREFIX_PATH CMAKE_ARGS AR AS LD NM RANLIB STRIP

BUILD_DIR="build"
JOBS="${JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

cmake -S . -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Release -DBOX3D_SAMPLES=ON
cmake --build "$BUILD_DIR" --target samples --config Release -- -j"$JOBS"

echo "Built build/bin/samples -- run it from the repo root to try the examples."
