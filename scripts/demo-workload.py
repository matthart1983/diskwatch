#!/usr/bin/env python3
"""Disk activity for recording the README demo.

Stages a small working tree under ROOT and keeps real programs busy in it
for a while: a server appending to its access log, a VM image written and
read at rates that drift up and down, a download (curl), a backup (tar and
zstd), a CI checkout being repacked (git) and a mirror (rsync). The rates are shaped
so the throughput graph has hills rather than a flat wall or lone spikes.

Everything is written under ROOT, which must not exist yet, and ROOT is
removed at the end. Use a directory on a real disk: on Fedora `/tmp` is
tmpfs, and writes there never reach the disk graph.

    python3 scripts/demo-workload.py ~/work --seconds 330
"""

import argparse
import functools
import http.server
import math
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

SENTINEL = ".diskwatch-demo"
MB = 1024 * 1024
# Random bytes, so btrfs or zfs compression can't make the writes vanish.
NOISE = os.urandom(16 * MB)

stop = threading.Event()


def wave(t, base, phase):
    """A rate in MB/s that drifts: a slow swell, a faster ripple, and a burst
    now and then, so the graph carries shape at every zoom."""
    slow = math.sin(2 * math.pi * (t + phase) / 26.0)
    fast = math.sin(2 * math.pi * (t + phase) / 7.5)
    burst = 2.2 if (int(t + phase) % 19) in (0, 1) else 0.0
    return max(0.0, base * (1.0 + 0.65 * slow + 0.3 * fast + burst))


def fill(path, size):
    with open(path, "wb") as f:
        left = size
        while left:
            n = min(left, len(NOISE))
            f.write(NOISE[:n])
            left -= n
        f.flush()
        os.fsync(f.fileno())


def vm_writer(img, base, start):
    """The VM image: random 1 MiB writes, fsynced every quarter second."""
    size = os.path.getsize(img)
    fd = os.open(img, os.O_WRONLY)
    try:
        while not stop.is_set():
            t0 = time.monotonic()
            want = int(wave(t0 - start, base, 0.0) * 0.25 * MB)
            while want > 0:
                n = min(want, MB)
                off = random.randrange(0, size - n) & ~4095
                at = random.randrange(0, len(NOISE) - n)
                os.pwrite(fd, NOISE[at : at + n], off)
                want -= n
            os.fsync(fd)
            stop.wait(max(0.0, 0.25 - (time.monotonic() - t0)))
    finally:
        os.close(fd)


def vm_reader(img, base, start):
    """Reads of the base image, with its pages dropped from the cache first
    so each read is a disk read."""
    size = os.path.getsize(img)
    fd = os.open(img, os.O_RDONLY)
    try:
        while not stop.is_set():
            t0 = time.monotonic()
            want = int(wave(t0 - start, base, 9.0) * 0.25 * MB)
            while want > 0:
                n = min(want, MB)
                off = random.randrange(0, size - n) & ~4095
                os.posix_fadvise(fd, off, n, os.POSIX_FADV_DONTNEED)
                os.pread(fd, n, off)
                want -= n
            stop.wait(max(0.0, 0.25 - (time.monotonic() - t0)))
    finally:
        os.close(fd)


def access_log(path, start):
    """A web server's access log, a few hundred lines a second, synced."""
    routes = ["/api/v1/orders", "/api/v1/users/me", "/healthz", "/api/v1/search?q=disk",
              "/static/app.js", "/api/v1/orders/8812/items"]
    with open(path, "a") as f:
        while not stop.is_set():
            rate = 120 + 220 * (0.5 + 0.5 * math.sin((time.monotonic() - start) / 5.0))
            for _ in range(int(rate / 5)):
                f.write(
                    f'10.0.{random.randrange(256)}.{random.randrange(256)} - - '
                    f'"GET {random.choice(routes)} HTTP/1.1" 200 {random.randrange(200, 9000)}\n'
                )
            f.flush()
            os.fsync(f.fileno())
            stop.wait(0.2)


def loop(cmd_fn, pause, procs):
    """Run a command again and again until told to stop."""
    while not stop.is_set():
        p = subprocess.Popen(cmd_fn(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(p)
        while p.poll() is None and not stop.is_set():
            time.sleep(0.1)
        if p.poll() is None:
            p.terminate()
        stop.wait(pause)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("root", type=Path)
    ap.add_argument("--seconds", type=float, default=300)
    ap.add_argument("--write-mb", type=float, default=45, help="mean VM write rate, MB/s")
    ap.add_argument("--read-mb", type=float, default=35, help="mean VM read rate, MB/s")
    args = ap.parse_args()

    root = args.root.expanduser().resolve()
    if root.exists():
        sys.exit(f"{root} already exists; pick a path that doesn't, it is removed at the end")
    repo = Path(__file__).resolve().parent.parent

    root.mkdir(parents=True)
    (root / SENTINEL).touch()
    for d in ["atlas-api/logs", "vm", "Downloads", "backups", "ci", "Pictures/2026-11"]:
        (root / d).mkdir(parents=True, exist_ok=True)
    for i in range(12):
        fill(root / f"Pictures/2026-11/IMG_{4810 + i}.HEIC", 4 * MB)
    fill(root / "vm/base.img", 384 * MB)
    fill(root / "vm/devbox.qcow2", 192 * MB)
    subprocess.run(["git", "clone", "-q", "--no-local", str(repo), str(root / "ci/diskwatch")],
                   check=True)

    # curl ignores --limit-rate for file://, so the "download" comes from a
    # local server instead.
    handler = functools.partial(
        type("Quiet", (http.server.SimpleHTTPRequestHandler,), {"log_message": lambda *a: None}),
        directory=str(root / "vm"),
    )
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    # A download cut off at shutdown is expected, not worth a traceback.
    server.handle_error = lambda *a: None
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/base.img"

    procs = []
    start = time.monotonic()
    threads = [
        threading.Thread(target=vm_writer, args=(root / "vm/devbox.qcow2", args.write_mb, start)),
        threading.Thread(target=vm_reader, args=(root / "vm/base.img", args.read_mb, start)),
        threading.Thread(target=access_log, args=(root / "atlas-api/logs/access.log", start)),
        threading.Thread(target=loop, args=(lambda: [
            "curl", "-s", "--limit-rate", "6M", "-o",
            str(root / "Downloads/fedora-44-x86_64.iso.part"), url], 4, procs)),
        threading.Thread(target=loop, args=(lambda: [
            "tar", "-I", "zstd -1 -T1", "-cf", str(root / "backups/photos-2026-11.tar.zst"),
            "-C", str(root), "Pictures"], 12, procs)),
        threading.Thread(target=loop, args=(lambda: [
            "git", "-C", str(root / "ci/diskwatch"), "repack", "-a", "-d", "-q"], 6, procs)),
        threading.Thread(target=loop, args=(lambda: [
            "rsync", "-a", "--ignore-times", "--inplace", "--bwlimit=6000",
            f"{root}/Pictures/", f"{root}/backups/mirror/"], 7, procs)),
    ]
    # vhs ends its shell when the tape does, and the hangup reaches this job
    # too. Stop cleanly on it, or ROOT is left behind.
    for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        signal.signal(sig, lambda *_: stop.set())
    for t in threads:
        t.start()
    stop.wait(args.seconds)
    stop.set()
    server.shutdown()
    for t in threads:
        t.join(timeout=10)
    for p in procs:
        if p.poll() is None:
            p.kill()
    if (root / SENTINEL).exists():
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
