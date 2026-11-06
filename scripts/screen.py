#!/usr/bin/env python3
"""Run a TUI in a pseudo-terminal, feed it keys, and print the final screen.

Used to compare wisp against yazi cell by cell:

    scripts/screen.py --dir ~/src --keys 'jjl' -- yazi
    scripts/screen.py --dir ~/src --keys 'jjl' -- target/release/wisp

Needs `pyte` (a VT100 emulator): `pip install pyte`. `--attrs` also prints a
second grid marking reversed/bold cells, to compare highlight bars.
Keys: plain characters, plus `<Esc>`, `<Enter>`, `<Tab>`, `<Up>`, `<Down>`,
`<Left>`, `<Right>`, `<C-x>` and `<wait>` (pause 300 ms).
"""
import argparse
import fcntl
import os
import pty
import re
import select
import struct
import termios
import time

import pyte

sys_path = os.path.dirname(os.path.abspath(__file__))
import sys

sys.path.insert(0, sys_path)
from bench import answer_queries  # noqa: E402

SPECIAL = {
    "Esc": b"\x1b",
    "Enter": b"\r",
    "Tab": b"\t",
    "Up": b"\x1b[A",
    "Down": b"\x1b[B",
    "Right": b"\x1b[C",
    "Left": b"\x1b[D",
    "Space": b" ",
    "BS": b"\x7f",
}


def parse_keys(s):
    out = []
    for m in re.finditer(r"<([^>]+)>|(.)", s, re.S):
        tok, ch = m.group(1), m.group(2)
        if ch is not None:
            out.append(ch.encode())
        elif tok == "wait":
            out.append(None)
        elif tok in SPECIAL:
            out.append(SPECIAL[tok])
        elif tok.startswith("C-") and len(tok) == 3:
            out.append(bytes([ord(tok[2].lower()) & 0x1F]))
        else:
            out.append(f"<{tok}>".encode())
    return out


def pump(fd, stream, secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.02)
        if r:
            try:
                data = os.read(fd, 1 << 16)
            except OSError:
                return
            if not data:
                return
            answer_queries(fd, data)
            stream.feed(data.decode("utf-8", "replace"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=".")
    ap.add_argument("--keys", default="")
    ap.add_argument("--rows", type=int, default=30)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--settle", type=float, default=0.8)
    ap.add_argument("--attrs", action="store_true")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    cwd = os.path.abspath(os.path.expanduser(a.dir))

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        env = dict(os.environ, TERM="xterm-256color", PWD=cwd)
        os.execvpe(cmd[0], cmd, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))
    screen = pyte.Screen(a.cols, a.rows)
    stream = pyte.Stream(screen)
    pump(fd, stream, a.settle)
    for k in parse_keys(a.keys):
        if k is None:
            pump(fd, stream, 0.3)
            continue
        os.write(fd, k)
        pump(fd, stream, 0.12)
    pump(fd, stream, a.settle)
    for line in screen.display:
        print(line.rstrip())
    if a.attrs:
        print("-" * a.cols)
        for y in range(a.rows):
            row = screen.buffer[y]
            s = ""
            for x in range(a.cols):
                c = row[x]
                s += "R" if c.reverse else ("B" if c.bold else ("u" if c.underscore else "."))
            print(s.rstrip("."))
    os.write(fd, b"\x1b")
    time.sleep(0.05)
    os.write(fd, b"q")
    time.sleep(0.2)
    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    main()
