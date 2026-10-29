#!/bin/bash
# End-to-end check of kitty animation playback against a real terminal.
#
# A client inside alacritty's PTY places a solid red image, overlays a green
# animation frame, and runs the animation (red 1.5 s, green 1.5 s). Screenshots
# of the placement area early and late in the cycle must show red, then green.

set -u
cd "$(dirname "$0")/.."

ALACRITTY=${ALACRITTY:-./target/debug/alacritty}
DISPLAY_ID=${DISPLAY_ID:-62}
export DISPLAY=:$DISPLAY_ID

CONFIG=$(mktemp /tmp/kitty-animation.XXXXXX.toml)
CLIENT=$(mktemp /tmp/kitty-animation-client.XXXXXX.py)
trap 'rm -f "$CLIENT"; pkill -f "Xvfb :$DISPLAY_ID" 2>/dev/null; [ -n "${ALACRITTY_PID:-}" ] && kill "$ALACRITTY_PID" 2>/dev/null' EXIT

cat > "$CLIENT" <<'EOF'
import os, time, base64
LOG = open("/tmp/kitty-animation-client.log", "w", buffering=1)
TTY = 0
def send(s):
    data = s.encode() if isinstance(s, str) else s
    view = memoryview(data)
    while view:
        view = view[os.write(TTY, view):]
def esc(control, data=b""):
    return f"\x1b_G{control};{base64.b64encode(data).decode()}\x1b\\"
def mark(name):
    open(f"/tmp/kitty-anim-{name}", "w").write("1")
    LOG.write(f"marked {name}\n")

time.sleep(1)
send(esc("a=T,i=9,f=32,s=100,v=100,c=8,r=4", bytes((255,0,0,255))*(100*100)))
mark("placed")
time.sleep(2)
send(esc("a=f,i=9,c=1,X=1,f=32,s=100,v=100,z=1500", bytes((0,255,0,255))*(100*100)))
mark("framed")
time.sleep(2)
send(esc("a=a,i=9,r=1,z=1500"))
send(esc("a=a,i=9,s=3,v=1"))
mark("running")
LOG.write("animation playing\n")
time.sleep(10)
EOF

pkill -f "Xvfb :$DISPLAY_ID" 2>/dev/null; sleep 0.5
Xvfb ":$DISPLAY_ID" -screen 0 1024x768x24 >/dev/null 2>&1 &
sleep 1.5

touch "$CONFIG"
env -u WAYLAND_DISPLAY DISPLAY="$DISPLAY" "$ALACRITTY" \
    -e python3 "$CLIENT" >/dev/null 2>&1 &
ALACRITTY_PID=$!
sleep 3

WINDOW=$(xdotool search --onlyvisible --class Alacritty 2>/dev/null | head -1)
[ -n "$WINDOW" ] && xdotool windowfocus "$WINDOW"

# Wait for the client to arm the animation, then sample the placement area:
# the red frame shows for the first 1.5 s, the green one for the next 1.5 s.
for _ in $(seq 1 30); do [ -f /tmp/kitty-anim-running ] && break; sleep 0.3; done
echo "client markers: $(ls /tmp/kitty-anim-* 2>/dev/null | tr '\n' ' ')"
sleep 0.4
DISPLAY="$DISPLAY" import -window root /tmp/kitty-anim-red.png 2>/dev/null
sleep 1.7
DISPLAY="$DISPLAY" import -window root /tmp/kitty-anim-green.png 2>/dev/null

sample() {
    convert "$1" -crop 60x70+6+6 -colorspace sRGB \
        -format "%[fx:int(255*mean.r)] %[fx:int(255*mean.g)] %[fx:int(255*mean.b)]" info: 2>/dev/null
}
read -r RED_R RED_G RED_B <<<"$(sample /tmp/kitty-anim-red.png)"
read -r GRN_R GRN_G GRN_B <<<"$(sample /tmp/kitty-anim-green.png)"
echo "red phase:  rgb($RED_R,$RED_G,$RED_B)"
echo "green phase: rgb($GRN_R,$GRN_G,$GRN_B)"

STATUS=1
if [ "${RED_R:-0}" -ge 150 ] && [ "${RED_G:-0}" -le 100 ] \
    && [ "${GRN_G:-0}" -ge 150 ] && [ "${GRN_R:-0}" -le 100 ]; then
    echo "animation verified: red flipped to green on schedule"
    STATUS=0
fi

kill "$ALACRITTY_PID" 2>/dev/null
rm -f /tmp/kitty-anim-running
exit $STATUS
