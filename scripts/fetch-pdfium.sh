#!/bin/bash
# PDFium (BSD-3-Clause), for PDF text and page renders: the prebuilt library from bblanchon/pdfium-binaries at a pinned
# release, in Vendor/pdfium (out of git). Does nothing when it's there already.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=chromium/8086
case "$(uname -m)" in
  x86_64) NAME=pdfium-linux-x64 ;;
  aarch64) NAME=pdfium-linux-arm64 ;;
  *) echo "no PDFium build known for $(uname -m)"; exit 1 ;;
esac
if [[ "$(cat Vendor/pdfium/TAG 2>/dev/null)" == "$TAG $NAME" ]]; then exit 0; fi
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
curl -sSfL -o "$T/pdfium.tgz" "https://github.com/bblanchon/pdfium-binaries/releases/download/${TAG/\//%2F}/$NAME.tgz"
rm -rf Vendor/pdfium && mkdir -p Vendor/pdfium && tar -xzf "$T/pdfium.tgz" -C Vendor/pdfium
echo "$TAG $NAME" > Vendor/pdfium/TAG
echo "PDFium $TAG in Vendor/pdfium"
