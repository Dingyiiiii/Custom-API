#!/usr/bin/env python3
"""Follow a libvirt domain's serial console for a while, without a TTY.

`virsh console` refuses to run when stdin is not a terminal, which makes it
useless from automation. The existing serial-console.py reads the pty in
non-blocking mode and returns whatever has arrived, so a loop of short reads
finishes instantly and misses everything — that is how a guest that was in fact
printing was mistaken for a dead one.

This keeps a single reader open and polls, so output is captured as it appears.
Input can be injected mid-stream with --send, which is what makes it usable for
answering a bootloader prompt.

  console-follow.py -d DOMAIN [--seconds N] [--send 'text' --at SECONDS]
                                [--until PATTERN]
"""

import argparse
import os
import select
import subprocess
import sys
import termios
import time


def pty_of(domain: str, script: str = "") -> str:
    """Return the domain's serial pty path.

    `virsh ttyconsole` is preferred: this script is run through sudo, where
    expanduser("~") resolves to /root and the helper's default path is wrong.
    """
    out = subprocess.run(
        ["virsh", "-c", "qemu:///system", "ttyconsole", domain],
        capture_output=True, text=True)
    if out.returncode == 0 and out.stdout.strip():
        return out.stdout.strip()
    if not script:
        raise SystemExit("could not determine the console device")
    out = subprocess.run(["python3", script, "-d", domain, "device"],
                         capture_output=True, text=True, check=True)
    return out.stdout.strip()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("-d", "--domain", default="customapi-freebsd")
    ap.add_argument("--script", default="")
    ap.add_argument("--seconds", type=float, default=60.0)
    ap.add_argument("--send", action="append", default=[],
                    help="text to write, may be repeated; \\n means newline")
    ap.add_argument("--at", type=float, default=20.0,
                    help="seconds after start at which --send is written")
    ap.add_argument("--until", default="",
                    help="stop early when this text appears")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    dev = pty_of(args.domain, args.script)
    print(f"[follow] console device {dev}, {args.seconds}s", file=sys.stderr)

    sink = open(args.out, "w") if args.out else sys.stdout
    fd = os.open(dev, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = 0
        attrs[1] = 0
        attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
        attrs[3] = 0
        attrs[6][termios.VMIN] = 0
        attrs[6][termios.VTIME] = 0
        termios.tcsetattr(fd, termios.TCSANOW, attrs)

        start = time.time()
        sent = False
        seen = ""
        while time.time() - start < args.seconds:
            elapsed = time.time() - start
            if args.send and not sent and elapsed >= args.at:
                for text in args.send:
                    os.write(fd, text.encode().decode("unicode_escape").encode())
                sent = True
            ready, _, _ = select.select([fd], [], [], 0.5)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                continue
            if not chunk:
                continue
            text = chunk.decode("utf-8", "replace")
            sink.write(text)
            sink.flush()
            seen += text
            if args.until and args.until in seen:
                print(f"\n[follow] matched {args.until!r} after {elapsed:.0f}s",
                      file=sys.stderr)
                break
    finally:
        os.close(fd)
        if args.out:
            sink.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
