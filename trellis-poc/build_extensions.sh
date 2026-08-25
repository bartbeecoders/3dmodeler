#!/usr/bin/env bash
# Build TRELLIS.2's CUDA extensions inside the trellis2 conda env.
set -ex
ROOT=/run/media/bart/Development/dev/bartbeecoders/3dModeler/trellis-poc
E=$ROOT/miniforge3/envs/trellis2
export CUDA_HOME=$E
export PATH=$E/bin:$PATH
export CC=$E/bin/x86_64-conda-linux-gnu-cc
export CXX=$E/bin/x86_64-conda-linux-gnu-c++
export NVCC_PREPEND_FLAGS="-ccbin $CXX"
export TORCH_CUDA_ARCH_LIST="8.9"
# conda-forge cuda-toolkit keeps headers/libs under targets/x86_64-linux
export CPATH=$E/targets/x86_64-linux/include${CPATH:+:$CPATH}
export LIBRARY_PATH=$E/targets/x86_64-linux/lib:$E/targets/x86_64-linux/lib/stubs:$E/lib${LIBRARY_PATH:+:$LIBRARY_PATH}
PIP=$E/bin/pip
mkdir -p $ROOT/extensions
cd $ROOT/extensions

echo "=== flash-attn (prebuilt wheel) ==="
$PIP show flash-attn >/dev/null 2>&1 || $PIP install "https://github.com/Dao-AILab/flash-attention/releases/download/v2.7.3/flash_attn-2.7.3+cu12torch2.6cxx11abiFALSE-cp310-cp310-linux_x86_64.whl"

echo "=== nvdiffrast ==="
[ -d nvdiffrast ] || git clone -b v0.4.0 https://github.com/NVlabs/nvdiffrast.git
$PIP show nvdiffrast >/dev/null 2>&1 || $PIP install ./nvdiffrast --no-build-isolation

echo "=== nvdiffrec ==="
[ -d nvdiffrec ] || git clone -b renderutils https://github.com/JeffreyXiang/nvdiffrec.git
$PIP install ./nvdiffrec --no-build-isolation

echo "=== CuMesh ==="
[ -d CuMesh ] || git clone --recursive https://github.com/JeffreyXiang/CuMesh.git
$PIP install ./CuMesh --no-build-isolation

echo "=== FlexGEMM ==="
[ -d FlexGEMM ] || git clone --recursive https://github.com/JeffreyXiang/FlexGEMM.git
$PIP install ./FlexGEMM --no-build-isolation

echo "=== o-voxel ==="
$PIP install $ROOT/TRELLIS.2/o-voxel --no-build-isolation

echo "ALL EXTENSIONS BUILT"
