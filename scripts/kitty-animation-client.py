#!/usr/bin/env python3
"""Runs inside alacritty's PTY: places a large red image, adds a green
animation frame over it, and runs the animation so the placement flips from
red to green once the root frame's gap has elapsed."""
import os
import sys
import time

LOG = open("/tmp/kitty-anim.log", "w", buffering=1)
TTY = 0  # the pty is the process's stdio

WIDTH = HEIGHT = 100


def log(msg):
    LOG.write(f"{msg}\n")


def send(data):
    view = memoryview(data)
    while view:
        view = view[os.write(TTY, view):]


def raw_escape(control, pixel_data=b""):
    import base64
    payload = base64.b64encode(pixel_data).decode()
    return f"\x1b_G{control};{payload}\x1b\\".encode()


def solid(pixel, count):
    return bytes(pixel) * count


log("waiting for go")
while not os.path.exists("/tmp/kitty-anim-go"):
    time.sleep(0.1)

# The root frame: a solid red 100x100 image, placed 8x4 cells at the cursor.
send(raw_escape(f"a=T,i=9,f=32,s={WIDTH},v={HEIGHT},c=8,r=4", solid((255, 0, 0, 255), WIDTH * HEIGHT)))
# Frame 2: a solid green full-image overwrite, held for 1.5 seconds.
send(raw_escape(f"a=f,i=9,c=1,X=1,f=32,s={WIDTH},v={HEIGHT},z=1000", solid((0, 255, 0, 255), WIDTH * HEIGHT)))
# 1.5 s of red, then 1.5 s of green, looping forever.
send(raw_escape("a=a,i=9,r=1,z=1000"))
send(raw_escape("a=a,i=9,s=3,v=1"))
log("animation armed")
with open("/tmp/kitty-anim-armed", "w") as done:
    done.write("armed")
# Stay alive while the harness screenshots the playback: the window closes
# when this process exits.
time.sleep(15)
