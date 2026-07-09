#!/usr/bin/env bash

# Use this to build Box3D on any system with a bash shell
rm -rf build

# Default to an optimized build: an empty CMAKE_BUILD_TYPE produces -O0 physics,
# which poisons every benchmark run against the library.
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
