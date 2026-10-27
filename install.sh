#!/bin/sh
# Build Kludd and install it for the current user (no root needed).
#
#   ./install.sh             build and install into ~/.local
#   ./install.sh --uninstall remove it again (your notes are never touched)
#
# PREFIX=/some/where ./install.sh installs elsewhere.
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_ID=io.github.loveranmar.Kludd

BIN="$PREFIX/bin/kludd"
DESKTOP="$PREFIX/share/applications/kludd.desktop"
ICON="$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
FONT_DIR="$DATA/fonts/kludd"

if [ "${1:-}" = "--uninstall" ]; then
    command -v kludd >/dev/null 2>&1 && kludd --quit 2>/dev/null || true
    rm -f "$BIN" "$DESKTOP" "$ICON"
    rm -rf "$FONT_DIR"
    fc-cache -f "$DATA/fonts" >/dev/null 2>&1 || true
    echo "Kludd removed. Notes in ~/Kludd and ~/.config/kludd are left alone."
    exit 0
fi

cd "$(dirname "$0")"
cargo build --release --locked

install -Dm755 target/release/kludd "$BIN"
# absolute Exec: ~/.local/bin is often not on the compositor's PATH
mkdir -p "$(dirname "$DESKTOP")"
sed "s|^Exec=kludd|Exec=$BIN|" assets/kludd.desktop > "$DESKTOP"
install -Dm644 "assets/$APP_ID.svg" "$ICON"
install -Dm644 "assets/fonts/Fredoka[wdth,wght].ttf" "$FONT_DIR/Fredoka[wdth,wght].ttf"
install -Dm644 assets/fonts/OFL.txt "$FONT_DIR/OFL.txt"
fc-cache -f "$DATA/fonts" >/dev/null 2>&1 || true
update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true

echo "Installed $BIN"
if ! fc-list | grep -q "JetBrains Mono"; then
    echo "Note: code and the status bar use JetBrains Mono: sudo pacman -S ttf-jetbrains-mono"
fi
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Note: $PREFIX/bin is not on your PATH." ;;
esac
if pgrep -x kludd >/dev/null 2>&1; then
    echo "Kludd is running the old version. Restart it with:"
    echo "  $BIN --quit; hyprctl dispatch exec '$BIN --hidden'"
fi
