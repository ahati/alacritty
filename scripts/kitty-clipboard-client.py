#!/usr/bin/env python3
"""Runs INSIDE alacritty's PTY: speaks the kitty clipboard protocol and logs
every reply the terminal writes back."""
import os
import select
import termios
import time
import tty

LOG = open("/tmp/kitty-clipboard.log", "w", buffering=1)
# The pty is the process's stdio; use it directly instead of a second open.
TTY = 0


def log(msg):
    LOG.write(f"{msg}\n")


def send(data):
    os.write(TTY, data)


def recv(deadline=2.0):
    buf = b""
    end = time.time() + deadline
    while time.time() < end:
        ready, _, _ = select.select([TTY], [], [], 0.1)
        if ready:
            buf += os.read(TTY, 65536)
    return buf


def exchange(name, data):
    log(f"{name} sent")
    send(data)
    reply = recv()
    log(f"{name} reply: [{reply.decode('utf-8', 'replace').replace(chr(27), 'ESC')}]")
    return reply


# Real protocol clients run the pty in raw mode; in canonical mode the
# terminal's newline-less replies would sit in the line buffer forever.
tty.setraw(0)
log("client started, waiting for go")
while not os.path.exists("/tmp/kitty-clipboard-go"):
    time.sleep(0.2)
log("go received")

# Sanity: the line discipline echoes our own bytes back when it is up.
send(b"echo-check\r")
log(f"echo check: [{recv(1.0)!r}]")

exchange(
    "write",
    b"\x1b]5522;type=write:id=t1\x1b\\"
    b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGVsbG8=\x1b\\"
    b"\x1b]5522;type=wdata\x1b\\",
)
exchange("list", b"\x1b]5522;type=read;Lg==\x1b\\")
exchange("read", b"\x1b]5522;type=read:id=r1;dGV4dC9wbGFpbg==\x1b\\")
exchange("decrqm", b"\x1b[?5522$p")
exchange("enosys-read", b"\x1b]5522;type=read;aW1hZ2UvcG5n\x1b\\")
# An unserved MIME alone is refused with ENOSYS.
assert b"ENOSYS" in exchange("enosys-read", b"\x1b]5522;type=read;aW1hZ2UvcG5n\x1b\\")
log("ENOSYS verified for an unserved MIME type")
# Round trip: the text written at the start must come back through a read.
reply = exchange("roundtrip-read", b"\x1b]5522;type=read:id=rt;dGV4dC9wbGFpbg==\x1b\\")
assert b"aGVsbG8=" in reply and b"OK" in reply and b"DONE:id=rt" in reply, "round trip failed"
log("round trip verified: hello written and read back")
log("sequence complete")
termios.tcsetattr(0, termios.TCSANOW, termios.tcgetattr(0))
with open("/tmp/kitty-clipboard-finished", "w") as done:
    done.write("done")
