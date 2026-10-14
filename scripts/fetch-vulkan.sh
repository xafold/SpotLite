#!/bin/bash
# Linux: what building llama.cpp for the GPU through Vulkan needs, without root: glslc, the Vulkan headers and
# SPIRV-Headers, from LunarG's Vulkan SDK (checked against its published SHA-256), kept in Vendor/vulkan (41 MB of the
# SDK's 1.9 GB). The program links the system's Vulkan loader (libvulkan1, which the NVIDIA, AMD and Intel drivers
# come with). scripts/build-llama.sh uses Vendor/vulkan when it's there. Does nothing when it's there already.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=1.4.363.0
if [[ "$(cat Vendor/vulkan/VERSION 2>/dev/null)" == "$VERSION" ]]; then exit 0; fi
LOADER=/usr/lib/x86_64-linux-gnu/libvulkan.so.1
[[ -f $LOADER ]] || { echo "no Vulkan loader at $LOADER (sudo apt install libvulkan1)"; exit 1; }
T=Vendor/vulkan-sdk
rm -rf "$T" && mkdir -p "$T"
NAME="vulkansdk-linux-x86_64-$VERSION.tar.xz"
curl -sSfL -o "$T/$NAME" "https://sdk.lunarg.com/sdk/download/$VERSION/linux/$NAME"
EXPECTED=$(curl -sSfL "https://sdk.lunarg.com/sdk/sha/$VERSION/linux/$NAME.txt" | cut -d' ' -f1)
[[ "$(sha256sum "$T/$NAME" | cut -d' ' -f1)" == "$EXPECTED" ]] || { echo "the Vulkan SDK arrived damaged"; exit 1; }
tar -xJf "$T/$NAME" -C "$T"
S="$T/$VERSION/x86_64"
rm -rf Vendor/vulkan && mkdir -p Vendor/vulkan/bin Vendor/vulkan/include Vendor/vulkan/lib Vendor/vulkan/share/cmake
cp "$S/bin/glslc" Vendor/vulkan/bin/
cp -r "$S/include/vulkan" "$S/include/vk_video" "$S/include/spirv" Vendor/vulkan/include/
cp -r "$S/share/cmake/SPIRV-Headers" Vendor/vulkan/share/cmake/
ln -s "$LOADER" Vendor/vulkan/lib/libvulkan.so
rm -rf "$T"
echo "$VERSION" > Vendor/vulkan/VERSION
echo "Vulkan build tools $VERSION in Vendor/vulkan"
