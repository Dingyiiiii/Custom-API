#!/bin/bash
# Custom-API FreeBSD VM control helpers (libvirt-managed, headless, serial console).
set -uo pipefail
VM=customapi-freebsd
SOCK=/tmp/customapi-console.sock
UR=(-c qemu:///system)

vs() { sudo -n virsh "${UR[@]}" "$@"; }

case "${1:-help}" in
  start)             vs start "$VM" ;;
  stop)              vs destroy "$VM"        ;;
  shutdown)          vs shutdown "$VM" || vs destroy "$VM" ;;
  state)             vs domstate "$VM" ;;
  list)              vs list --all ;;
  ip)                vs domifaddr "$VM" --source lease | awk 'NR>2{print $4}' | cut -d/ -f1 ;;
  attach)
      # Bridge the serial pty to a unix socket so that readers and writers can
      # attach without needing a controlling TTY, which `virsh console`
      # requires and automation cannot provide.
      pkill -f "socat.*$SOCK" 2>/dev/null
      rm -f "$SOCK"
      TTY=$(vs ttyconsole "$VM")
      echo "serial tty: $TTY"
      sudo -n chmod 666 "$TTY"
      sudo -n nohup socat UNIX-LISTEN:$SOCK,fork,mode=666 "FILE:$TTY,raw,echo=0,nonblock" \
           >/tmp/customapi-socat.log 2>&1 &
      sleep 1.5
      [ -S "$SOCK" ] && echo "console socket ready: $SOCK" || { echo "socat FAILED"; cat /tmp/customapi-socat.log; }
      ;;
  send)  # send raw text to console
      printf '%b' "$2" | socat - UNIX-CONNECT:$SOCK ;;
  read)  # read console output for N seconds
      timeout "${2:-5}" socat - UNIX-CONNECT:$SOCK | cat -v ;;
  shell) # run a command non-interactively and print output
      cmd="$2"; wait="${3:-6}"
      socat -T "$wait" - UNIX-CONNECT:$SOCK <<< "$cmd" ;;
  *) sed -n '2,12p' "$0" ;;
esac
