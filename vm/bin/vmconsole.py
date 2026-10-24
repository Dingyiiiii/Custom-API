#!/usr/bin/env python3
"""Drive the libvirt serial console of the Custom-API FreeBSD VM.

Usage:
  vmconsole.py send "<command>"      # send a line, print output
  vmconsole.py cmd "<command>"       # send, wait for a shell prompt marker
  vmconsole.py read [seconds]        # just dump console output
"""
import os, pty, sys, time, select, termios, errno

TTYFILE = "/tmp/customapi-console.tty"

def tty_path():
    if os.path.exists(TTYFILE):
        p = open(TTYFILE).read().strip()
        if os.path.exists(p):
            return p
    return None

def set_ttyfile(path):
    open(TTYFILE, "w").write(path)

def open_console():
    p = tty_path()
    if not p:
        sys.exit("no console tty recorded; run: vmconsole.py attach-path")
    fd = os.open(p, os.O_RDWR | os.O_NOCTTY)
    old = termios.tcgetattr(fd)
    new = termios.tcgetattr(fd)
    new[0] = 0                                   # iflag: raw
    new[1] = 0                                   # oflag
    new[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    new[3] = 0                                   # lflag: no echo/canon
    new[6][termios.VMIN] = 0
    new[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, new)
    return fd, old

def drain(fd, seconds, quiet=False):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError as e:
                if e.errno in (errno.EIO, errno.EAGAIN):
                    break
                raise
            if not chunk:
                break
            out += chunk
    txt = out.decode("utf-8", "replace")
    if not quiet and txt:
        sys.stdout.write(txt)
        sys.stdout.flush()
    return txt

def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    op = sys.argv[1]
    if op == "attach-path":
        print(f"# set by: virsh ttyconsole customapi-freebsd")
        return
    if op == "read":
        secs = float(sys.argv[2]) if len(sys.argv) > 2 else 5
        fd, old = open_console()
        try:
            drain(fd, secs)
        finally:
            termios.tcsetattr(fd, termios.TCSANOW, old)
            os.close(fd)
        return
    if op in ("send", "cmd"):
        line = sys.argv[2]
        wait = float(sys.argv[3]) if len(sys.argv) > 3 else (12 if op == "cmd" else 4)
        fd, old = open_console()
        try:
            drain(fd, 1.0, quiet=True)              # flush stale
            os.write(fd, b"\n")
            time.sleep(0.4)
            drain(fd, 1.0, quiet=True)
            os.write(fd, line.encode() + b"\n")
            txt = drain(fd, wait)
            return txt
        finally:
            termios.tcsetattr(fd, termios.TCSANOW, old)
            os.close(fd)

if __name__ == "__main__":
    main()
