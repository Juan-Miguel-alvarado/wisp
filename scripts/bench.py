#!/usr/bin/env python3
"""Measure how long a terminal file manager takes to show a directory.

Spawns the program in a fresh pseudo-terminal of a fixed size, records the time
from `exec` until a marker filename shows up in its output (i.e. the listing was
actually painted), then sends `q` and waits for exit. Repeats N times and prints
min / median, so two file managers can be compared on the same directory:

    scripts/bench.py --dir /tmp/big --marker file_000001 -- yazi
    scripts/bench.py --dir /tmp/big --marker file_000001 -- target/release/wisp

`--keys` additionally measures a navigation round trip: after the first paint,
it sends the keys and waits for a second marker.
"""
import argparse
import os
import pty
import select
import signal
import statistics
import struct
import fcntl
import termios
import time


QUERIES = [
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b[5n", b"\x1b[0n"),
    (b"\x1b[>q", b"\x1bP>|xterm(390)\x1b\\"),
    (b"\x1b[16t", b"\x1b[6;20;10t"),
    (b"\x1b[14t", b"\x1b[4;800;1600t"),
    (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1bP$q q\x1b\\", b"\x1bP1$r2 q\x1b\\"),
    (b"\x1b[?12$p", b"\x1b[?12;2$y"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b[0c", b"\x1b[?62;22c"),
    (b"\x1b[c", b"\x1b[?62;22c"),
]


def answer_queries(fd, chunk):
    found = []
    for q, a in QUERIES:
        start = 0
        while True:
            i = chunk.find(q, start)
            if i < 0:
                break
            found.append((i, a))
            start = i + len(q)
    for _, a in sorted(found):
        os.write(fd, a)


def spawn(cmd, cwd, rows, cols):
    """Fork `cmd` onto a fresh pty that already has its final size, so the
    program never sees a 0x0 terminal and our fork cost is not billed to it."""
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["PWD"] = cwd
    env.pop("TMUX", None)
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for fd in (0, 1, 2):
            os.dup2(slave, fd)
        if slave > 2:
            os.close(slave)
        os.chdir(cwd)
        os.execvpe(cmd[0], cmd, env)
    os.close(slave)
    return pid, master


def run_once(cmd, cwd, marker, rows, cols, timeout, keys=None, marker2=None):
    pid, fd = spawn(cmd, cwd, rows, cols)
    start = time.perf_counter()
    buf = b""
    first = second = None
    deadline = start + timeout
    need = marker.encode()
    while time.perf_counter() < deadline:
        r, _, _ = select.select([fd], [], [], 0.01)
        if not r:
            continue
        try:
            chunk = os.read(fd, 1 << 16)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
        # Answer terminal queries the way a real (xterm-like) terminal does, in
        # order, so programs that probe the terminal do not stall waiting on us.
        answer_queries(fd, chunk)
        if first is None and need in buf:
            first = time.perf_counter() - start
            if keys is None:
                break
            buf = b""
            t2 = time.perf_counter()
            os.write(fd, keys.encode())
            need = marker2.encode()
            continue
        if first is not None and second is None and need in buf:
            second = time.perf_counter() - t2
            break
    rss = read_rss(pid)
    try:
        os.write(fd, b"q")
        end = time.perf_counter() + 2
        while time.perf_counter() < end:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                break
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                try:
                    os.read(fd, 1 << 16)
                except OSError:
                    pass
        else:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
    except OSError:
        pass
    os.close(fd)
    return first, second, rss


def read_rss(pid):
    """Resident memory of the process tree rooted at `pid`, in MiB."""
    total = 0
    pids = [pid]
    try:
        for d in os.listdir("/proc"):
            if d.isdigit():
                try:
                    with open(f"/proc/{d}/stat") as f:
                        ppid = int(f.read().rsplit(")", 1)[1].split()[1])
                    if ppid == pid:
                        pids.append(int(d))
                except OSError:
                    pass
        for p in pids:
            with open(f"/proc/{p}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        total += int(line.split()[1])
    except OSError:
        return None
    return total / 1024


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True)
    ap.add_argument("--marker", required=True)
    ap.add_argument("--keys")
    ap.add_argument("--marker2")
    ap.add_argument("-n", type=int, default=10)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--cols", type=int, default=160)
    ap.add_argument("--timeout", type=float, default=20)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    firsts, seconds, rsss = [], [], []
    for _ in range(a.n):
        f, s, rss = run_once(cmd, a.dir, a.marker, a.rows, a.cols, a.timeout, a.keys, a.marker2)
        if rss is not None:
            rsss.append(rss)
        if f is None:
            print("timeout waiting for marker")
            continue
        firsts.append(f * 1000)
        if s is not None:
            seconds.append(s * 1000)
        time.sleep(0.05)
    if firsts:
        print(f"{' '.join(cmd)}: first paint  min {min(firsts):7.1f} ms  median {statistics.median(firsts):7.1f} ms")
    if seconds:
        print(f"{' '.join(cmd)}: after keys   min {min(seconds):7.1f} ms  median {statistics.median(seconds):7.1f} ms")
    if rsss:
        print(f"{' '.join(cmd)}: memory (RSS) median {statistics.median(rsss):7.1f} MiB")


if __name__ == "__main__":
    main()
