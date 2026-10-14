#!/bin/bash
# Installs SpotLite for this user, without root: the app and the engine with the libraries they run on, in
# ~/.local/lib/spotlite; SpotLite in the app menu and at login; `spotlite` in ~/.local/bin (search from a terminal); and
# the shortcut Ctrl+Shift+Space on COSMIC and GNOME (elsewhere, bind `spotlite-app --panel` to a key yourself: the
# app's own hotkey only reaches X11 windows). Run it again to update: it replaces what it installed.
# From a release download, it installs the files beside it; from a clone, what `cargo build --release` built there.
# A DigUp for Linux installed before is moved over: its index, model, settings and shortcut.
# With --cli, only the `spotlite` command (the engine and its libraries): no app, tray, panel or shortcut.
set -euo pipefail
CLI=; [[ ${1:-} == --cli ]] && CLI=1
HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HOME/.local/lib/spotlite"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
shopt -s nullglob

if [[ -x $HERE/spotlite-app ]]; then   # a release download
  FILES=("$HERE/spotlite-app" "$HERE/spotlite" "$HERE"/lib*.so*)
  ICON="$HERE/spotlite.png"
else                                   # a clone, built
  cd "$HERE/.."
  needed=(engine/target/release/spotlite)
  [[ $CLI ]] || needed+=(app/src-tauri/target/release/spotlite-app)
  for built in "${needed[@]}"; do
    [[ -x $built ]] || { echo "build it first: no $built (see README.md, Build from source)"; exit 1; }
  done
  [[ $CLI ]] || scripts/fetch-layer-shell.sh || echo "no gtk-layer-shell: the panel will be a plain window"
  FILES=(app/src-tauri/target/release/spotlite-app engine/target/release/spotlite Vendor/llama/lib/*.so*
         Vendor/pdfium/lib/libpdfium.so Vendor/gtk-layer-shell/*.so*)
  ICON=app/src-tauri/icons/128x128.png
fi
if [[ $CLI ]]; then   # the engine and the libraries only
  engine_files=()
  for file in "${FILES[@]}"; do [[ $file == *spotlite-app ]] || engine_files+=("$file"); done
  FILES=("${engine_files[@]}")
fi

# What it runs on, from the system (Ubuntu/Debian names).
missing=()
has_library() { ldconfig -p 2> /dev/null | grep -F "$1" > /dev/null; }   # all of it read: no broken pipe
if [[ ! $CLI ]]; then
  has_library libwebkit2gtk-4.1.so.0 || missing+=(libwebkit2gtk-4.1-0)
  has_library libayatana-appindicator3.so.1 || missing+=(libayatana-appindicator3-1)
fi
has_library libgomp.so.1 || missing+=(libgomp1)   # llama.cpp's threads: the engine can't start without it
has_library libvulkan.so.1 || missing+=(libvulkan1)   # the GPU; without it, the CPU
command -v ffmpeg > /dev/null || missing+=(ffmpeg)
command -v tesseract > /dev/null || missing+=(tesseract-ocr)
if (( ${#missing[@]} )); then
  echo "Install these first (tesseract-ocr is optional: the words in screenshots and scans):"
  echo "  sudo apt install ${missing[*]}"
  has_library libgomp.so.1 || exit 1
  [[ $CLI ]] || { has_library libwebkit2gtk-4.1.so.0 && has_library libayatana-appindicator3.so.1; } || exit 1
fi

# DigUp for Linux before it: SpotLite takes over its index and model (moved, not copied), settings and shortcut.
if [[ ! $CLI ]]; then
  pkill -x digup-shell 2>/dev/null || true
  pkill -x spotlite-app 2>/dev/null || true
  if [[ -d $DATA/digup && ! -e $DATA/spotlite ]]; then mv "$DATA/digup" "$DATA/spotlite"; echo "moved DigUp's index and model"; fi
  if [[ -d $CONFIG/com.abdurrahim.DigUp && ! -e $CONFIG/com.xafold.spotlite ]]; then
    mv "$CONFIG/com.abdurrahim.DigUp" "$CONFIG/com.xafold.spotlite"
  fi
  rm -rf "$HOME/.local/lib/digup" "$DATA/com.abdurrahim.DigUp" "$DATA/applications/digup.desktop" "$CONFIG/autostart/digup.desktop"
  [[ -L $HOME/.local/bin/digup ]] && rm -f "$HOME/.local/bin/digup"
fi

# A running SpotLite has let go of its files; copies are renamed into place, so a running binary is never written to.
mkdir -p "$DEST"
put() { cp -a "$1" "$DEST/.$(basename "$1").new" && mv -f "$DEST/.$(basename "$1").new" "$DEST/$(basename "$1")"; }
for file in "${FILES[@]}"; do put "$file"; done
# `spotlite search …` in a terminal (and for agents): the engine on the PATH, beside its libraries through the link.
mkdir -p "$HOME/.local/bin"
ln -sfn "$DEST/spotlite" "$HOME/.local/bin/spotlite"
if [[ $CLI ]]; then
  echo "spotlite installed in $DEST, linked from ~/.local/bin/spotlite. Next:"
  echo "  spotlite download-model                 # the model, about 865 MB, once"
  echo "  spotlite index ~/Documents ~/Pictures   # your folders; run it again to read new and changed files"
  echo "  spotlite search \"what you remember\""
  exit 0
fi
cp "$ICON" "$DEST/spotlite.png"

# WebKitGTK 2.52 spins forever (a blank window) on web fonts its font cache lists without a style: a fresh cache
# lists them properly.
[[ -d $DATA/fonts ]] && fc-cache -f "$DATA/fonts" || true

ENTRY="[Desktop Entry]
Type=Application
Name=SpotLite
Comment=Search your files by what's in them
Icon=$DEST/spotlite.png
Terminal=false
Categories=Utility;
StartupNotify=false"
mkdir -p "$DATA/applications" "$CONFIG/autostart"
printf '%s\nExec=%s --panel\n' "$ENTRY" "$DEST/spotlite-app" > "$DATA/applications/spotlite.desktop"
printf '%s\nExec=%s\nX-GNOME-Autostart-enabled=true\n' "$ENTRY" "$DEST/spotlite-app" > "$CONFIG/autostart/spotlite.desktop"

# The shortcut. COSMIC: beside any custom ones already there (a line of SpotLite's or DigUp's is replaced).
SHORTCUTS="$CONFIG/cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom"
if [[ -d $CONFIG/cosmic ]]; then
  LINE="    (modifiers: [Ctrl, Shift], key: \"space\"): Spawn(\"$DEST/spotlite-app --panel\"),"
  mkdir -p "$(dirname "$SHORTCUTS")"
  if [[ ! -s $SHORTCUTS ]]; then
    printf '{\n%s\n}\n' "$LINE" > "$SHORTCUTS"
  else
    grep -v -e 'spotlite-app' -e 'digup-shell' "$SHORTCUTS" | sed '$d' > "$SHORTCUTS.new"   # all but ours and the "}"
    printf '%s\n}\n' "$LINE" >> "$SHORTCUTS.new"
    mv "$SHORTCUTS.new" "$SHORTCUTS"
  fi
  echo "shortcut: Ctrl+Shift+Space (COSMIC Settings → Keyboard → Custom Shortcuts)"
# GNOME: a custom keybinding of its own, added to the list.
elif command -v gsettings > /dev/null && gsettings list-schemas 2>/dev/null | grep -q media-keys; then
  KEYS=org.gnome.settings-daemon.plugins.media-keys
  ENTRY_PATH=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/spotlite/
  LIST=$(gsettings get $KEYS custom-keybindings)
  if [[ $LIST != *"$ENTRY_PATH"* ]]; then
    if [[ $LIST == "@as []" || $LIST == "[]" ]]; then LIST="['$ENTRY_PATH']"; else LIST="${LIST%]}, '$ENTRY_PATH']"; fi
    gsettings set $KEYS custom-keybindings "$LIST"
  fi
  BINDING="$KEYS.custom-keybinding:$ENTRY_PATH"
  gsettings set "$BINDING" name 'SpotLite'
  gsettings set "$BINDING" command "$DEST/spotlite-app --panel"
  gsettings set "$BINDING" binding '<Control><Shift>space'
  echo "shortcut: Ctrl+Shift+Space (Settings → Keyboard → Custom Shortcuts)"
else
  echo "shortcut: bind \`$DEST/spotlite-app --panel\` to a key in your desktop's keyboard settings"
fi

setsid "$DEST/spotlite-app" > /dev/null 2>&1 < /dev/null &
echo "SpotLite installed in $DEST and started (it lives in the tray). The first start downloads the model (~865 MB)."
