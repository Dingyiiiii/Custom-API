#!/usr/bin/env python3
"""Drive a libvirt serial console without a controlling TTY.

`virsh console` refuses to run when stdin is not a terminal, which makes it
unusable from automation. This script talks to the domain's serial pty directly
instead, so it works over a non-interactive SSH session and on hosts that do not
have socat or expect installed.

It is deliberately dependency-free (Python standard library only) and only needs
read access to the pty, which it obtains through `virsh ttyconsole` plus sudo.

Subcommands
-----------
  device                     print the console pty path
  read [SECONDS]             dump raw console output for SECONDS (default 5)
  send TEXT                  write TEXT literally (use $'\\n' style escapes by
                             passing --escapes), then dump whatever arrives
  run COMMAND [WAIT]         send a command line and capture its output for
                             WAIT seconds (default 10)
  login USER [PASSWORD]      wait for a login prompt, log in, and confirm a
                             shell prompt
  waitfor PATTERN [TIMEOUT]  wait until PATTERN (a regular expression) appears
                             in the console output, then exit 0; exit 1 on
                             timeout

Typical use
-----------
  serial-console.py login root freebsd
  serial-console.py run 'uname -a' 5
  serial-console.py waitfor 'login:' 120

Exit status is 0 on success and non-zero when a requested pattern was not seen,
so the caller can branch on it.
"""

from __future__ import annotations

import argparse
import errno
import os
import pty
import re
import select
import subprocess
import sys
import termios
import time

DOMAIN_DEFAULT = "customapi-freebsd"
VIRSH = "virsh"
URI = "qemu:///system"


def run_quiet(cmd: list[str]) -> str:
    """Run a command, returning stripped stdout, or '' when it fails."""
    try:
        out = subprocess.run(
            cmd, capture_output=True, text=True, timeout=30, check=False
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    if out.returncode != 0:
        return ""
    return out.stdout.strip()


def console_device(domain: str) -> str:
    """Return the pty path backing the domain's serial console."""
    path = run_quiet(["sudo", "-n", VIRSH, "-c", URI, "ttyconsole", domain])
    if not path:
        path = run_quiet([VIRSH, "-c", URI, "ttyconsole", domain])
    if not path:
        sys.exit(
            f"serial-console: no console device for domain {domain!r}; "
            "is the domain running and does it have a serial device?"
        )
    return path.splitlines()[-1].strip()


class Console:
    """A raw-mode file descriptor for the console pty."""

    def __init__(self, device: str):
        # O_NOCTTY keeps the pty from becoming this process's controlling
        # terminal, which would make it receive SIGINT from the SSH session.
        try:
            self.fd = os.open(device, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        except PermissionError:
            sys.exit(
                f"serial-console: cannot open {device}: permission denied; "
                f"try: sudo chmod 666 {device}"
            )
        except OSError as exc:
            sys.exit(f"serial-console: cannot open {device}: {exc}")
        self._save = termios.tcgetattr(self.fd)
        attrs = termios.tcgetattr(self.fd)
        # Fully raw: no echo, no canonical processing, no signal generation.
        # Without this, getty's output arrives with CRLF translation and the
        # patterns below would need to account for terminal processing.
        attrs[0] = 0  # iflag
        attrs[1] = 0  # oflag
        attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
        attrs[3] = 0  # lflag
        attrs[6][termios.VMIN] = 0
        attrs[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, attrs)

    def close(self) -> None:
        try:
            termios.tcsetattr(self.fd, termios.TCSANOW, self._save)
            os.close(self.fd)
        except OSError:
            pass

    def write(self, data: str | bytes) -> None:
        if isinstance(data, str):
            data = data.encode()
        os.write(self.fd, data)

    def read_for(self, seconds: float) -> str:
        """Collect console output for a fixed period."""
        chunks: list[bytes] = []
        deadline = time.monotonic() + seconds
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            ready, _, _ = select.select([self.fd], [], [], min(remaining, 0.2))
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError as exc:
                if exc.errno in (errno.EIO, errno.EAGAIN):
                    break
                raise
            if not data:
                break
            chunks.append(data)
        return b"".join(chunks).decode("utf-8", "replace")

    def wait_for(
        self, pattern: re.Pattern[str], timeout: float, settle: float = 0.4
    ) -> tuple[bool, str]:
        """Wait for a regexp to appear, returning (found, transcript)."""
        buf: list[str] = []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            buf.append(self.read_for(0.3))
            text = "".join(buf)
            if pattern.search(text):
                # Give the guest a moment to finish printing the rest of the
                # response so callers see a complete transcript.
                buf.append(self.read_for(settle))
                return True, "".join(buf)
        return False, "".join(buf)


def strip_echo(transcript: str, sent: str) -> str:
    """Remove the echoed copy of what we typed from a transcript."""
    out = []
    for line in transcript.splitlines():
        if sent and sent.strip() and sent.strip() in line and len(line.strip()) <= len(sent.strip()) + 4:
            continue
        out.append(line)
    return "\n".join(out)


def cmd_device(args: argparse.Namespace) -> int:
    print(console_device(args.domain))
    return 0


def cmd_read(args: argparse.Namespace) -> int:
    con = Console(console_device(args.domain))
    try:
        sys.stdout.write(con.read_for(args.seconds))
    finally:
        con.close()
    return 0


def cmd_send(args: argparse.Namespace) -> int:
    text = args.text
    if args.escapes:
        # Match the shell's $'...' convention for convenience at the call site.
        text = text.encode().decode("unicode_escape")
    con = Console(console_device(args.domain))
    try:
        con.write(text)
        sys.stdout.write(con.read_for(args.wait))
    finally:
        con.close()
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    con = Console(console_device(args.domain))
    try:
        # Wake the console first: getty only prints its prompt once it sees
        # input, so an idle console would otherwise look dead.
        con.write(b"\n")
        con.read_for(0.4)
        con.write(args.command.encode() + b"\n")
        sys.stdout.write(con.read_for(args.wait))
    finally:
        con.close()
    return 0


def cmd_login(args: argparse.Namespace) -> int:
    con = Console(console_device(args.domain))
    login_re = re.compile(rb"(?i)(login:|Password:)")
    try:
        # Nudge the console until a prompt appears.
        prompt_seen = False
        deadline = time.monotonic() + args.timeout
        transcript = ""
        con.write(b"\n")
        while time.monotonic() < deadline:
            got, text = con.wait_for(re.compile(r"(?i)login:"), 5)
            transcript += text
            if got:
                prompt_seen = True
                break
        if not prompt_seen:
            sys.stdout.write(transcript)
            print("\nserial-console: no login prompt appeared", file=sys.stderr)
            return 1

        con.write(args.user.encode() + b"\n")
        if args.password:
            got, text = con.wait_for(re.compile(r"(?i)password:"), 15)
            transcript += text
            if not got:
                sys.stdout.write(transcript)
                print("\nserial-console: no password prompt appeared", file=sys.stderr)
                return 1
            con.write(args.password.encode() + b"\n")
        # A shell prompt is '#' for root and '$' for a normal user.
        got, text = con.wait_for(re.compile(r"[#$]\s*$", re.M), 20, settle=0.6)
        transcript += text
        sys.stdout.write(transcript)
        if not got:
            print("\nserial-console: logged in but no shell prompt appeared", file=sys.stderr)
            return 1
        return 0
    finally:
        con.close()


def cmd_waitfor(args: argparse.Namespace) -> int:
    con = Console(console_device(args.domain))
    try:
        found, transcript = con.wait_for(re.compile(args.pattern), args.timeout)
        if args.verbose or not found:
            sys.stdout.write(transcript)
        return 0 if found else 1
    finally:
        con.close()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Drive a libvirt serial console without a TTY.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "-d", "--domain", default=DOMAIN_DEFAULT,
        help=f"libvirt domain name (default: {DOMAIN_DEFAULT})",
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("device", help="print the console pty path")
    p.set_defaults(func=cmd_device)

    p = sub.add_parser("read", help="dump console output")
    p.add_argument("seconds", nargs="?", type=float, default=5.0)
    p.set_defaults(func=cmd_read)

    p = sub.add_parser("send", help="write text to the console")
    p.add_argument("text")
    p.add_argument("--wait", type=float, default=3.0)
    p.add_argument("--escapes", action="store_true",
                   help=r"interpret \n and \t in TEXT")
    p.set_defaults(func=cmd_send)

    p = sub.add_parser("run", help="send a command line and capture output")
    p.add_argument("command")
    p.add_argument("wait", nargs="?", type=float, default=10.0)
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("login", help="log in at the console")
    p.add_argument("user")
    p.add_argument("password", nargs="?", default="")
    p.add_argument("--timeout", type=float, default=180.0)
    p.set_defaults(func=cmd_login)

    p = sub.add_parser("waitfor", help="wait for a pattern in the console output")
    p.add_argument("pattern")
    p.add_argument("timeout", nargs="?", type=float, default=120.0)
    p.add_argument("-v", "--verbose", action="store_true")
    p.set_defaults(func=cmd_waitfor)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
