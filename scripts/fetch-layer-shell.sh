#!/bin/bash
# Linux: gtk-layer-shell, which lets the panel be a layer-shell surface (what launchers use on COSMIC, KDE, Sway and
# Hyprland): the compositor gives it the height it asks for and draws no window border around it. Without root: the
# distribution's package is downloaded (apt checks it against the signed archive) and its library kept in
# Vendor/gtk-layer-shell, which install.sh copies next to the app. The shell loads it when it runs and works
# without it (a plain window). Does nothing when it's there already.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ -f Vendor/gtk-layer-shell/libgtk-layer-shell.so.0 ]] && exit 0
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
(cd "$T" && apt-get download libgtk-layer-shell0 >/dev/null)
dpkg-deb -x "$T"/libgtk-layer-shell0_*.deb "$T/root"
mkdir -p Vendor/gtk-layer-shell
cp -L "$T"/root/usr/lib/*/libgtk-layer-shell.so.0 Vendor/gtk-layer-shell/
echo "gtk-layer-shell in Vendor/gtk-layer-shell"
