#!/bin/sh
#
# Operate the Custom-API guest on laboratory.lan.
#
# The guest is deliberately headless with serial-only access, so every
# interaction goes through the libvirt serial console. This wrapper hides the
# SSH hop and the remote path so the console commands read naturally:
#
#   ./vm.sh start | stop | status | console
#   ./vm.sh device                    # the console pty on the remote host
#   ./vm.sh read [SECONDS]
#   ./vm.sh run 'COMMAND' [WAIT]
#   ./vm.sh login [USER] [PASSWORD]
#   ./vm.sh waitfor 'PATTERN' [TIMEOUT]
#   ./vm.sh push LOCAL REMOTE         # copy a file into the guest over HTTP
#   ./vm.sh serve [PORT]              # serve the release directory on this host
#
# Environment:
#   LAB_HOST   SSH destination of the libvirt host (default administrator@laboratory.lan)
#   LAB_VM     libvirt domain name (default customapi-freebsd)
#   LAB_DIR    working directory on the libvirt host (default ~/customapi-vm)

set -eu

LAB_HOST=${LAB_HOST:-administrator@laboratory.lan}
LAB_VM=${LAB_VM:-customapi-freebsd}
LAB_DIR=${LAB_DIR:-'$HOME/customapi-vm'}
SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "${SELF_DIR}/.." && pwd)

remote() { ssh -o BatchMode=yes "${LAB_HOST}" "$@"; }
console() {
	remote "cd ${LAB_DIR} && sudo -n python3 scripts/serial-console.py -d ${LAB_VM} $*"
}

case "${1:-help}" in
start)
	remote "sudo -n virsh -c qemu:///system start ${LAB_VM}" ;;
stop)
	remote "sudo -n virsh -c qemu:///system shutdown ${LAB_VM} || sudo -n virsh -c qemu:///system destroy ${LAB_VM}" ;;
status)
	remote "sudo -n virsh -c qemu:///system list --all; echo; sudo -n virsh -c qemu:///system domifaddr ${LAB_VM} 2>/dev/null || true" ;;
console)
	# A genuinely interactive console needs a TTY; -t is what makes it work.
	exec ssh -t "${LAB_HOST}" "sudo -n virsh -c qemu:///system console ${LAB_VM}" ;;
device)
	console device ;;
read)
	shift; console "read ${1:-5}" ;;
run)
	shift; console "run '$1' ${2:-10}" ;;
send)
	shift; console "send '$1' --escapes" ;;
login)
	shift; console "login ${1:-root} ${2:-customapi}" ;;
waitfor)
	shift; console "waitfor '$1' ${2:-120}" ;;
serve)
	# Serve the staged release over HTTP so the guest can pull it. The guest is
	# on the same LAN as this host, so no NAT or port forwarding is involved.
	PORT=${2:-8899}
	IP=$(ip -4 -br addr show | awk -v p="${PORT}" '$1!="lo"{split($3,a,"/"); print a[1]; exit}')
	echo "serving ${ROOT}/dist on http://${IP}:${PORT}/"
	exec python3 -m http.server "${PORT}" --bind "${IP}" --directory "${ROOT}/dist"
	;;
push)
	# Copy a local file to the guest by serving it briefly and fetching it over
	# the console. Used for artifacts the guest cannot download itself.
	LOCAL=$2; REMOTE_TARGET=$3; PORT=${4:-8899}
	IP=$(ip -4 -br addr show | awk '$1!="lo"{split($3,a,"/"); print a[1]; exit}')
	DIR=$(dirname "${LOCAL}"); BASE=$(basename "${LOCAL}")
	( cd "${DIR}" && python3 -m http.server "${PORT}" --bind "${IP}" >/dev/null 2>&1 & echo $! > /tmp/customapi-serve.pid )
	sleep 1
	console "run 'fetch -q -o ${REMOTE_TARGET} http://${IP}:${PORT}/${BASE} && echo FETCH_OK && wc -c < ${REMOTE_TARGET}' 25"
	kill "$(cat /tmp/customapi-serve.pid 2>/dev/null)" 2>/dev/null || true
	;;
*)
	sed -n '2,30p' "$0" ;;
esac
