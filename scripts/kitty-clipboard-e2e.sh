#!/bin/bash
# End-to-end check of the kitty clipboard protocol (OSC 5522) against a real
# terminal.
#
# A client program runs inside alacritty's PTY, speaks the protocol, and
# asserts on the replies the terminal writes back: a chunked write, the MIME
# listing, a data read with the `id` echoed, the DECRQM answer of the
# paste-events mode, ENOSYS for an unserved MIME type, and the round trip of
# the written text coming back through a read.
#
# Requires Xvfb, xdotool and python3. The window is focused before the
# exchange because the clipboard is only touched for a focused terminal.

set -u
cd "$(dirname "$0")/.."

ALACRITTY=${ALACRITTY:-./target/debug/alacritty}
DISPLAY_ID=${DISPLAY_ID:-99}
export DISPLAY=:$DISPLAY_ID

CLIENT=$(mktemp /tmp/kitty-clipboard-client.XXXXXX.py)
CONFIG=$(mktemp /tmp/kitty-clipboard.XXXXXX.toml)
ALACRITTY_PID=
trap 'rm -f "$CLIENT" "$CONFIG" /tmp/kitty-clipboard-go /tmp/kitty-clipboard-finished /tmp/kitty-clipboard.log; [ -n "$ALACRITTY_PID" ] && kill "$ALACRITTY_PID" 2>/dev/null; pkill -f "Xvfb :$DISPLAY_ID" 2>/dev/null' EXIT

cat > "$CONFIG" <<'EOF'
[terminal.kitty_clipboard]
read = "allow"
write = "allow"
EOF

# The client asserts the whole exchange and writes the finish marker.
sed -e 's|/tmp/client.log|/tmp/kitty-clipboard.log|' \
    -e 's|/tmp/go|/tmp/kitty-clipboard-go|' \
    -e 's|/tmp/finished|/tmp/kitty-clipboard-finished|' \
    scripts/kitty-clipboard-client.py > "$CLIENT"

pkill -f "Xvfb :$DISPLAY_ID" 2>/dev/null; sleep 0.5
Xvfb ":$DISPLAY_ID" -screen 0 1024x768x24 >/dev/null 2>&1 &
sleep 1.5

env -u WAYLAND_DISPLAY DISPLAY="$DISPLAY" "$ALACRITTY" --config-file "$CONFIG" \
    -e python3 "$CLIENT" >/dev/null 2>&1 &
ALACRITTY_PID=$!
sleep 3

# Focus the terminal; clipboard access is gated on focus like OSC 52's.
WINDOW=$(xdotool search --onlyvisible --class Alacritty 2>/dev/null | head -1)
[ -n "$WINDOW" ] && xdotool windowfocus "$WINDOW"
touch /tmp/kitty-clipboard-go

for _ in $(seq 1 40); do
    [ -f /tmp/kitty-clipboard-finished ] && break
    sleep 0.5
done

cat /tmp/kitty-clipboard.log 2>/dev/null
STATUS=$?
kill "$ALACRITTY_PID" 2>/dev/null
exit $STATUS
