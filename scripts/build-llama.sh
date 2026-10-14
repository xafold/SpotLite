#!/bin/bash
# Builds llama.cpp (MIT), the embedding runtime, at the pinned tag with scripts/llama.cpp-no-logits.patch, as shared
# libraries in Vendor/llama/lib with the headers in Vendor/llama/include (all out of git). ggml's backends are modules
# loaded at run time (GGML_BACKEND_DL): every CPU variant always (one build fits any x86-64 machine), and Vulkan when
# glslc is found (scripts/fetch-vulkan.sh puts it in Vendor/vulkan without root), or CUDA when nvcc is (CUDACXX, or on
# PATH; CUDA_ARCHS picks the GPU architectures, default native). Source paths are written into the libraries as
# relative ones (-ffile-prefix-map), and they find each other beside themselves ($ORIGIN), so a build says nothing
# about the machine it was made on. CMake comes from PATH,
# or from a venv in Vendor/ (needs uv). Does nothing when that build is already there.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=b11461   # the first tags with EmbeddingGemma 2 (text + vision + audio) are b11452+
COMMIT=4d756bc72bf00a4aacf410ae15a2d315f3db400d   # TAG's commit: a tag can be moved, a commit can't
# A new TAG changes the vectors only if its numbers differ; if they do, bump VECTOR_VERSION in engine/src/embed.rs, so
# indexes embed again rather than mix vectors.
PATCH=scripts/llama.cpp-no-logits.patch
if [[ -x Vendor/vulkan/bin/glslc ]]; then
  export VULKAN_SDK="$PWD/Vendor/vulkan" PATH="$PWD/Vendor/vulkan/bin:$PATH" CMAKE_PREFIX_PATH="$PWD/Vendor/vulkan"
fi
SOURCE="${LLAMA_SOURCE:-Vendor/llama.cpp}"
BUILD="${LLAMA_BUILD:-Vendor/llama-build}"
OUT=Vendor/llama
if command -v "${CUDACXX:-nvcc}" > /dev/null; then
  FLAVOR=cuda
elif command -v glslc > /dev/null; then
  FLAVOR=vulkan
else
  FLAVOR=cpu
fi
STAMP="$TAG $(sha256sum "$PATCH" | cut -c1-12) $FLAVOR relative-paths origin-rpath"
if [[ -f "$OUT/lib/libllama.so" && "$(cat "$OUT/BUILD" 2>/dev/null)" == "$STAMP" ]]; then
  exit 0
fi

if [[ ! -d "$SOURCE" ]]; then
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$SOURCE"
elif [[ "$(git -C "$SOURCE" describe --tags --exact-match 2>/dev/null)" != "$TAG" ]]; then
  git -C "$SOURCE" fetch --quiet --depth 1 origin tag "$TAG"
  git -C "$SOURCE" checkout --quiet --force "$TAG"   # --force also drops the old patch
fi
if [[ "$(git -C "$SOURCE" rev-parse HEAD)" != "$COMMIT" ]]; then
  echo "llama.cpp's $TAG isn't commit $COMMIT here: not building it" >&2
  exit 1
fi
# Without the patch, every input token costs 1 MiB of logits that an embedding model never produces (262k vocabulary).
if git -C "$SOURCE" apply --check "$PWD/$PATCH" 2>/dev/null; then
  git -C "$SOURCE" apply "$PWD/$PATCH"
fi

if ! command -v cmake > /dev/null; then
  if [[ ! -x Vendor/cmake-venv/bin/cmake ]]; then
    uv venv --quiet Vendor/cmake-venv --python 3.12
    uv pip install --quiet --python Vendor/cmake-venv/bin/python cmake==4.4.4 ninja==1.13.2
  fi
  export PATH="$PWD/Vendor/cmake-venv/bin:$PATH"
fi
GENERATOR=$(command -v ninja > /dev/null && echo Ninja || echo "Unix Makefiles")
case $FLAVOR in
  cuda) GPU=(-DGGML_CUDA=ON "-DCMAKE_CUDA_ARCHITECTURES=${CUDA_ARCHS:-native}") ;;
  vulkan) GPU=(-DGGML_VULKAN=ON) ;;
  *) GPU=() ;;
esac
RELATIVE="-ffile-prefix-map=$PWD/=./ -ffile-prefix-map=$PWD=."
cmake -S "$SOURCE" -B "$BUILD" -G "$GENERATOR" -DCMAKE_BUILD_TYPE=Release \
  "-DCMAKE_C_FLAGS=$RELATIVE" "-DCMAKE_CXX_FLAGS=$RELATIVE" \
  -DCMAKE_SKIP_BUILD_RPATH=ON -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON '-DCMAKE_INSTALL_RPATH=$ORIGIN' \
  -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON -DGGML_CPU_ALL_VARIANTS=ON -DGGML_NATIVE=OFF "${GPU[@]}" \
  -DLLAMA_BUILD_MTMD=ON -DLLAMA_BUILD_COMMON=OFF -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF \
  -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_APP=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_OPENSSL=OFF \
  -DLLAMA_CURL=OFF > /dev/null
cmake --build "$BUILD" --config Release -j "$(nproc)" > /dev/null
rm -rf "$OUT/lib" && mkdir -p "$OUT/lib" "$OUT/include"
# The libraries with their versioned names (the SONAMEs the engine links against) and the symlinks to them.
cp -a "$BUILD"/bin/*.so* "$OUT/lib/"
cp "$SOURCE/include/llama.h" "$SOURCE"/ggml/include/*.h "$SOURCE/tools/mtmd/mtmd.h" "$SOURCE/tools/mtmd/mtmd-helper.h" \
  "$OUT/include/"
echo "$STAMP" > "$OUT/BUILD"
echo "Built llama.cpp $TAG (patched, $FLAVOR) into $OUT"
