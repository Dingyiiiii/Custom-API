#!/bin/bash
#
# Restore the Custom-API FreeBSD guest from the retired CentOS disk.
#
# The old server's system disk is mounted read-only at $SRC_MNT (its LVM volume
# group cs_laboratory). The guest that ran there is copied back rather than
# rebuilt, because rebuilding means the base image's first-boot base-system
# upgrade, which takes 30-60 minutes and silences the serial console.
#
# Run on the new lab host as root.
#
#   ./lab/restore-lab-vm.sh --source /mnt/oldcs [--name customapi-freebsd]
#
# Two details are easy to get wrong and are handled explicitly:
#
#   * the working disk is copied only when it is missing or a stub - an earlier
#     attempt silently overwrote the recovered disk with a fresh one, which
#     looks like a guest that boots but has lost everything;
#   * the MAC is pinned in the domain XML. libvirt generates a random address
#     otherwise, and the LAN's DHCP server holds a reservation that binds a
#     specific address to this guest's name.
set -euo pipefail

SOURCE=/mnt/oldcs
NAME=customapi-freebsd
MAC=52:54:00:19:44:f8
NETWORK=external-macvtap
IMAGE_DIR=/var/lib/libvirt/images/customapi
WORKDIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

while [ $# -gt 0 ]; do
	case "$1" in
	--source) SOURCE="$2"; shift 2 ;;
	--name) NAME="$2"; shift 2 ;;
	--mac) MAC="$2"; shift 2 ;;
	--network) NETWORK="$2"; shift 2 ;;
	-h|--help) sed -n '3,20p' "$0"; exit 0 ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
virsh_cmd() { sudo -n virsh -c qemu:///system "$@"; }

SRC_IMG_DIR="${SOURCE}/var/lib/libvirt/images/customapi"
[ -d "${SRC_IMG_DIR}" ] || die "${SRC_IMG_DIR} not found; is the old disk mounted?"

log "source: ${SRC_IMG_DIR}"
sudo -n ls -l "${SRC_IMG_DIR}/" | sed 's/^/    /'

# ------------------------------------------------------------------- images
install -d -o qemu -g qemu -m 0755 "${IMAGE_DIR}"

recover() {
	src="$1"; dst="$2"; want="$3"
	if [ ! -f "${dst}" ]; then
		action=copy
	elif [ "$(stat -c %s "${dst}")" -lt "${want}" ]; then
		# A stub here means a previous run created a fresh disk over the
		# recovered one. Copying again is the whole point of this script.
		action=replace
	else
		action=keep
	fi
	case "${action}" in
	copy|replace)
		log "$action $(basename "${dst}") ($(stat -c %s "${src}") bytes)"
		sudo -n rm -f "${dst}"
		sudo -n cp "${src}" "${dst}"
		sudo -n chown qemu:qemu "${dst}"
		;;
	keep)
		log "keeping existing $(basename "${dst}") ($(stat -c %s "${dst}") bytes)"
		;;
	esac
}

# The working disk is the guest's whole state; require it to be substantial.
recover "${SRC_IMG_DIR}/${NAME}.qcow2" "${IMAGE_DIR}/${NAME}.qcow2" 100000000
recover "${SRC_IMG_DIR}/${NAME}-seed.iso" "${IMAGE_DIR}/${NAME}-seed.iso" 100000

log "verifying the recovered disk"
sudo -n qemu-img check "${IMAGE_DIR}/${NAME}.qcow2" 2>&1 | tail -2 | sed 's/^/    /'

# ------------------------------------------------------------------ network
if ! virsh_cmd net-info "${NETWORK}" >/dev/null 2>&1; then
	log "defining libvirt network ${NETWORK} on eno1"
	virsh_cmd net-define "${WORKDIR}/${NETWORK}.xml" >/dev/null
	virsh_cmd net-start "${NETWORK}" >/dev/null
	virsh_cmd net-autostart "${NETWORK}" >/dev/null
else
	log "network ${NETWORK} already defined"
fi

# ------------------------------------------------------------------- domain
if virsh_cmd dominfo "${NAME}" >/dev/null 2>&1; then
	log "removing the existing domain definition"
	virsh_cmd destroy "${NAME}" >/dev/null 2>&1 || true
	virsh_cmd undefine "${NAME}" --nvram >/dev/null 2>&1 \
		|| virsh_cmd undefine "${NAME}" >/dev/null 2>&1 || true
fi

# Start from the recovered definition when it exists, so the guest keeps the
# hardware it was configured with; fall back to the checked-in template.
XML_SRC="${SRC_IMG_DIR}/../../../../etc/libvirt/qemu/${NAME}.xml"
if [ -f "${XML_SRC}" ]; then
	log "using the recovered domain definition"
	cp "${XML_SRC}" "${WORKDIR}/${NAME}.recovered.xml"
	XML="${WORKDIR}/${NAME}.recovered.xml"
else
	log "using the checked-in domain template"
	XML="${WORKDIR}/${NAME}.xml"
fi

# Pin the MAC: a generated address would not match the LAN's reservation.
python3 - "${XML}" "${MAC}" "${NETWORK}" "${IMAGE_DIR}" "${NAME}" <<'PY'
import re, sys
path, mac, network, image_dir, name = sys.argv[1:6]
s = open(path).read()
# Drop any existing mac, then set ours on the first interface.
s = re.sub(r"\s*<mac address='[^']*'/>", "", s)
s = re.sub(r"(<interface type='network'>)",
           r"\1\n      <mac address='%s'/>" % mac, s, count=1)
# Point the disks at the recovered files regardless of what the source said.
s = re.sub(r"<source file='[^']*%s\.qcow2'/>" % re.escape(name),
           "<source file='%s/%s.qcow2'/>" % (image_dir, name), s)
s = re.sub(r"<source file='[^']*%s-seed\.iso'/>" % re.escape(name),
           "<source file='%s/%s-seed.iso'/>" % (image_dir, name), s)
s = re.sub(r"<source network='[^']*'/>", "<source network='%s'/>" % network, s)
open(path, 'w').write(s)
print("    mac=%s network=%s" % (mac, network))
PY

log "defining and starting ${NAME}"
virsh_cmd define "${XML}" >/dev/null
virsh_cmd start "${NAME}" >/dev/null

sleep 6
log "state"
virsh_cmd list --all | sed 's/^/    /'
log "interface actually in use"
virsh_cmd dumpxml "${NAME}" | grep -oE "mac address='[0-9a-f:]+'" | head -1 | sed 's/^/    /'
