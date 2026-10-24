# VM helper scripts

These complement `../create-vm.sh`, which is the entry point for creating and
managing the libvirt-hosted FreeBSD test VM.

| Script | Purpose |
|---|---|
| `vmctl.sh` | start/stop/inspect the domain, and bridge the serial console to a unix socket |
| `vmconsole.py` | drive the serial console directly (pty + termios raw mode), for when no TTY is available |

## Serial console without a TTY

`virsh console` refuses to run without a controlling terminal, which breaks
automation and any agent-driven workflow. Both helpers work around that:

```sh
bin/vmctl.sh attach           # bridge the serial pty to /tmp/customapi-console.sock
bin/vmctl.sh read 5           # read five seconds of console output
bin/vmctl.sh send 'uname -a\n'
bin/vmctl.sh ip               # report the guest's leased address

bin/vmconsole.py read 10      # equivalent, using Python's pty module
```

The console is on `ttyu0` with `console="comconsole,vidconsole"` set in the
image's `/boot/loader.conf`, so kernel messages and the login prompt both appear
there.

Two behaviours surprise people, so they are worth stating:

- **An idle console prints nothing.** `getty` emits its prompt only after it
  sees input, so `read` on a booted, idle guest legitimately returns nothing.
  Send a newline first.
- **A single read can miss the reply.** Input and output race within one
  connection; sending newlines repeatedly and waiting a few seconds is reliable.

A working exchange looks like this:

```sh
$ ( for i in 1 2 3; do printf '\n'; sleep 2; done ) | socat -T 12 - UNIX-CONNECT:/tmp/customapi-console.sock
^M
root@freebsd:~ # ^M
root@freebsd:~ # ^M
root@freebsd:~ #
```
