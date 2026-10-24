#!/bin/sh
#
# Create the Custom-API FreeBSD test VM under libvirt.
#
# The VM is headless, runs as an unprivileged libvirt domain (no host device
# passthrough, user-mode networking), and is reachable through the serial
# console:
#
#	virsh -c qemu:///system console customapi-freebsd
#
# It is also reachable over SSH once provisioning has run.
#
# Usage:
#   ./vm/create-vm.sh              create (or recreate) the network and domain
#   ./vm/create-vm.sh status       show domain and network state
#   ./vm/create-vm.sh start|stop|console|ssh
#   ./vm/create-vm.sh console      attach to the serial console
#   ./vm/create-vm.sh baseline     install the baseline packages inside the VM
#   ./vm/create-vm.sh destroy      undefine the domain and networks (keeps images)
#   ./vm/create-vm.sh purge        also remove the downloaded and installed images
#
# Every download goes through a domestic (CN) mirror; see $MIRROR below.

set -eu

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

VM=customapi-freebsd
NET=customapi-net
NET_BRIDGE=virbr-capi
NET_CIDR=10.77.30.0/24
VM_IP=10.77.30.31
VM_MAC=52:54:00:ca:91:01
VM_MEM_MIB=4096
VM_VCPUS=4
VM_DISK_GB=60

# Domestic mirrors. USTC carries both the release images and the pkg repo.
MIRROR=https://mirrors.ustc.edu.cn/freebsd
FBSD_VERSION=15.1-RELEASE
IMAGE_XZ=FreeBSD-${FBSD_VERSION}-amd64-BASIC-CLOUDINIT-ufs.qcow2.xz
IMAGE_BASE="FreeBSD-${FBSD_VERSION}-amd64-BASIC-CLOUDINIT-ufs.qcow2"

# libvirt's image store: QEMU runs as the unprivileged `qemu` user, which
# cannot traverse a 0700 home directory, so images cannot live in the workspace.
IMAGE_DIR=/var/lib/libvirt/images/customapi

# VM_SSH_ALIAS is the SSH config alias the tooling uses.
VM_SSH_ALIAS=customapi-bsd

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "${SELF_DIR}/.." && pwd)

# ---------------------------------------------------------------------------

log()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_root() {
	[ "$(id -u)" = "0" ] || sudo -n true 2>/dev/null || die "root (or passwordless sudo) is required"
}

virsh_cmd() { sudo -n virsh -c qemu:///system "$@"; }

cmd_status() {
	echo "--- domain"
	virsh_cmd list --all
	echo "--- network"
	virsh_cmd net-list --all
	echo "--- address"
	virsh_cmd domifaddr "$VM" --source lease 2>/dev/null || true
	echo "--- serial console device"
	virsh_cmd ttyconsole "$VM" 2>/dev/null || echo "(domain not running)"
	echo
	echo "Attach with: virsh -c qemu:///system console $VM"
	echo "SSH with:    ssh $VM_SSH_ALIAS"
}

# --- network ---------------------------------------------------------------

create_network() {
	if virsh_cmd net-info "$NET" >/dev/null 2>&1; then
		log "network $NET already defined"
	else
		log "defining network $NET"
		cat > /tmp/${NET}.xml <<EOF
<network>
  <name>${NET}</name>
  <forward mode='nat'>
    <nat>
      <port start='1024' end='65535'/>
    </nat>
  </forward>
  <bridge name='${NET_BRIDGE}' stp='on' delay='0'/>
  <dns enable='no'/>
  <ip address='10.77.30.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='10.77.30.10' end='10.77.30.200'/>
      <host mac='${VM_MAC}' name='${VM}' ip='${VM_IP}'/>
    </dhcp>
  </ip>
</network>
EOF
		virsh_cmd net-define /tmp/${NET}.xml
		virsh_cmd net-autostart "$NET"
	fi
	virsh_cmd net-start "$NET" 2>/dev/null || true

	# The host firewall must allow the guest to reach a local HTTP server used
	# for first-boot provisioning. Without this the guest gets "connection
	# refused" even though it can ping the bridge.
	if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
		if ! firewall-cmd --zone=libvirt --list-ports 2>/dev/null | grep -q '8899/tcp'; then
			log "opening tcp/8899 to the libvirt zone (provisioning HTTP server)"
			sudo -n firewall-cmd --permanent --zone=libvirt --add-port=8899/tcp >/dev/null
			sudo -n firewall-cmd --reload >/dev/null
		fi
	fi
}

# --- image -----------------------------------------------------------------

fetch_image() {
	sudo -n install -d -m 0755 -o qemu -g qemu "${IMAGE_DIR}"

	if [ ! -f "${IMAGE_DIR}/${IMAGE_BASE}" ]; then
		if [ ! -f "${SELF_DIR}/${IMAGE_XZ}" ]; then
			log "downloading ${IMAGE_XZ} from ${MIRROR}"
			curl -fL --retry 3 -o "${SELF_DIR}/${IMAGE_XZ}" \
				"${MIRROR}/releases/VM-IMAGES/${FBSD_VERSION}/amd64/Latest/${IMAGE_XZ}" \
				|| die "download failed"
		fi
		log "verifying and extracting the image"
		xz -t "${SELF_DIR}/${IMAGE_XZ}" || die "the downloaded image is corrupt"
		xz -dk -T0 -c "${SELF_DIR}/${IMAGE_XZ}" | sudo -n tee "${IMAGE_DIR}/${IMAGE_BASE}" >/dev/null
	else
		log "base image already present"
	fi

	sudo -n chown qemu:qemu "${IMAGE_DIR}/${IMAGE_BASE}"
	sudo -n chmod 0644 "${IMAGE_DIR}/${IMAGE_BASE}"
}

# --- first-boot provisioning ------------------------------------------------

build_seed() {
	KEYFILE=${HOME}/.ssh/id_ed25519.pub
	if [ ! -f "${KEYFILE}" ]; then
		log "generating an SSH key for VM access"
		ssh-keygen -t ed25519 -N '' -f "${HOME}/.ssh/id_ed25519" >/dev/null
	fi
	SSH_PUBKEY=$(cat "${KEYFILE}")

	log "building the cloud-init seed"
	cat > "${SELF_DIR}/meta-data" <<EOF
instance-id: customapi-freebsd-01
local-hostname: customapi-bsd
EOF

	# FreeBSD's nuageinit (the native cloud-init) provisions the guest. The
	# service user and the application itself are installed later by the
	# deploy tooling; this seed only has to produce a reachable machine with a
	# working sudo.
	cat > "${SELF_DIR}/user-data" <<EOF
#cloud-config
hostname: customapi-bsd
timezone: Asia/Shanghai
disable_root: false
ssh_pwauth: true
users:
  - name: agent
    gecos: Agent
    groups: [wheel, operator]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    shell: /bin/sh
    lock_passwd: false
    ssh_authorized_keys:
      - ${SSH_PUBKEY}
write_files:
  - path: /etc/ssh/sshd_config.d/10-agent.conf
    content: |
      PermitRootLogin prohibit-password
      PasswordAuthentication yes
      PubkeyAuthentication yes
      UseDNS no
runcmd:
  - [ sh, -c, "sysrc sshd_enable=YES" ]
  - [ sh, -c, "service sshd restart || service sshd start" ]
  - [ sh, -c, "printf '%s\\\\n' 'nameserver 223.5.5.5' 'nameserver 119.29.29.29' > /etc/resolv.conf" ]
  - [ sh, -c, "mkdir -p /usr/local/etc/pkg/repos" ]
  - [ sh, -c, "printf '%s\\\\n' 'FreeBSD: { enabled: no }' > /usr/local/etc/pkg/repos/FreeBSD.conf" ]
  - [ sh, -c, "printf '%s\\\\n' 'ustc: {' '  url: \\"pkg+https://mirrors.ustc.edu.cn/freebsd-pkg/\\\${ABI}/quarterly\\",' '  mirror_type: \\"srv\\",' '  signature_type: \\"fingerprints\\",' '  fingerprints: \\"/usr/share/keys/pkg\\",' '  enabled: yes' '}' > /usr/local/etc/pkg/repos/ustc.conf" ]
  - [ sh, -c, "sysrc dumpdev=NO" ]
  - [ sh, -c, "sysrc growfs_enable=YES" ]
final_message: "CUSTOMAPI-CLOUD-INIT-COMPLETE after \\\$UPTIME seconds"
EOF

	command -v mkisofs >/dev/null 2>&1 || die "mkisofs is required to build the seed ISO"
	(cd "${SELF_DIR}" && mkisofs -quiet -output seed.iso -volid CIDATA -joliet -rock \
		user-data meta-data) || die "failed to build the seed ISO"
	sudo -n install -m 0444 -o qemu -g qemu "${SELF_DIR}/seed.iso" "${IMAGE_DIR}/seed.iso"
}

prepare_disk() {
	log "creating a ${VM_DISK_GB} GiB working disk from the base image"
	sudo -n rm -f "${IMAGE_DIR}/${VM}.qcow2"
	sudo -n qemu-img create -f qcow2 -F qcow2 -b "${IMAGE_DIR}/${IMAGE_BASE}" \
		"${IMAGE_DIR}/${VM}.qcow2" "${VM_DISK_GB}G" >/dev/null
	# growfs in rc.conf expands the UFS root partition to the full disk on boot.
	sudo -n qemu-img resize "${IMAGE_DIR}/${VM}.qcow2" "${VM_DISK_GB}G" >/dev/null
	sudo -n chown qemu:qemu "${IMAGE_DIR}/${VM}.qcow2"
	sudo -n chmod 0644 "${IMAGE_DIR}/${VM}.qcow2"
	log "disk ready: ${IMAGE_DIR}/${VM}.qcow2"
}

# --- domain -----------------------------------------------------------------

create_domain() {
	if virsh_cmd dominfo "$VM" >/dev/null 2>&1; then
		log "undefining the existing domain"
		virsh_cmd destroy "$VM" 2>/dev/null || true
		virsh_cmd undefine "$VM" --nvram 2>/dev/null || virsh_cmd undefine "$VM"
	fi

	log "writing the domain definition"
	cat > "${SELF_DIR}/${VM}.xml" <<EOF
<domain type='kvm'>
  <name>${VM}</name>
  <title>Custom-API FreeBSD test environment (headless, serial console)</title>
  <description>FreeBSD ${FBSD_VERSION} non-privileged test VM for the Custom-API gateway project. Managed by libvirt; console via: virsh -c qemu:///system console ${VM}</description>
  <memory unit='MiB'>${VM_MEM_MIB}</memory>
  <currentMemory unit='MiB'>${VM_MEM_MIB}</currentMemory>
  <vcpu placement='static'>${VM_VCPUS}</vcpu>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <boot dev='hd'/>
    <boot dev='cdrom'/>
    <bootmenu enable='no'/>
  </os>
  <features>
    <acpi/>
    <apic/>
  </features>
  <cpu mode='host-passthrough' check='none' migratable='off'>
    <topology sockets='1' dies='1' cores='${VM_VCPUS}' threads='1'/>
  </cpu>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>restart</on_crash>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' cache='writeback' discard='unmap'/>
      <source file='${IMAGE_DIR}/${VM}.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='${IMAGE_DIR}/seed.iso'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <controller type='virtio-serial' index='0'/>
    <interface type='network'>
      <mac address='${VM_MAC}'/>
      <source network='${NET}'/>
      <model type='virtio'/>
    </interface>
    <serial type='pty'>
      <target type='isa-serial' port='0'/>
    </serial>
    <console type='pty'>
      <target type='serial' port='0'/>
    </console>
    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
    <memballoon model='none'/>
  </devices>
</domain>
EOF
	virsh_cmd define "${SELF_DIR}/${VM}.xml" >/dev/null
	log "domain defined"
}

configure_ssh() {
	CONFIG=${HOME}/.ssh/config
	[ -f "${CONFIG}" ] || { mkdir -p "${HOME}/.ssh"; touch "${CONFIG}"; chmod 600 "${CONFIG}"; }
	if grep -q "Host ${VM_SSH_ALIAS}\$" "${CONFIG}" 2>/dev/null; then
		log "SSH alias ${VM_SSH_ALIAS} already configured"
		return
	fi
	log "adding SSH alias ${VM_SSH_ALIAS}"
	cat >> "${CONFIG}" <<EOF

Host ${VM_SSH_ALIAS}
    HostName ${VM_IP}
    User agent
    IdentityFile ~/.ssh/id_ed25519
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    ServerAliveInterval 30
    LogLevel ERROR
EOF
}

wait_for_ssh() {
	log "waiting for the guest to accept SSH (this can take a couple of minutes on first boot)"
	_deadline=$(( $(date +%s) + ${1:-420} ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		if ssh -o BatchMode=yes -o ConnectTimeout=8 "${VM_SSH_ALIAS}" true 2>/dev/null; then
			log "guest is reachable"
			return 0
		fi
		sleep 5
	done
	return 1
}

cmd_create() {
	require_root
	create_network
	fetch_image
	prepare_disk
	build_seed
	create_domain
	configure_ssh

	log "starting ${VM}"
	virsh_cmd start "$VM"
	if wait_for_ssh 480; then
		log "VM is up"
		cmd_status
	else
		warn "the guest did not become reachable over SSH within the timeout"
		warn "attach to the serial console to investigate:"
		warn "  virsh -c qemu:///system console $VM"
		exit 1
	fi
}

# --- baseline packages ------------------------------------------------------

cmd_baseline() {
	log "installing baseline packages inside the guest (via the USTC mirror)"
	# The toolchain is installed *inside* the guest on purpose. The acceptance
	# test only needs python3/jq/curl, but a native `cargo build` here is the
	# escape hatch when the host has no FreeBSD cross toolchain: rustup can add
	# the target, but linking still needs a compiler for the target system.
	ssh "${VM_SSH_ALIAS}" 'sudo -n env ASSUME_ALWAYS_YES=yes pkg install -y \
		curl jq ca_root_nss sudo python3 rust git 2>&1 | tail -4'
	ssh "${VM_SSH_ALIAS}" 'echo "--- versions"; \
		freebsd-version; \
		/usr/local/bin/rustc --version 2>/dev/null || echo "rustc: missing"; \
		/usr/local/bin/cargo --version 2>/dev/null || echo "cargo: missing"; \
		/usr/local/bin/python3 -V 2>/dev/null || echo "python3: missing"; \
		/usr/local/bin/jq --version 2>/dev/null || echo "jq: missing"'
	log "baseline installed"
}

cmd_console() {
	exec sudo -n virsh -c qemu:///system console "$VM"
}

cmd_ssh() {
	exec ssh "${VM_SSH_ALIAS}"
}

cmd_destroy() {
	require_root
	virsh_cmd destroy "$VM" 2>/dev/null || true
	virsh_cmd undefine "$VM" --nvram 2>/dev/null || virsh_cmd undefine "$VM" 2>/dev/null || true
	log "domain undefined (images kept in ${IMAGE_DIR})"
}

cmd_purge() {
	cmd_destroy
	log "removing images from ${IMAGE_DIR}"
	sudo -n rm -f "${IMAGE_DIR}/${VM}.qcow2" "${IMAGE_DIR}/seed.iso"
	log "removing the downloaded archive from the workspace"
	rm -f "${SELF_DIR}/${IMAGE_XZ}"
	# Keep the base image: it is the expensive artifact and is reused by create.
	log "done (base image ${IMAGE_BASE} kept for reuse)"
}

# ---------------------------------------------------------------------------

case "${1:-create}" in
create)   cmd_create ;;
status)   cmd_status ;;
start)    virsh_cmd start "$VM" ;;
stop)     virsh_cmd shutdown "$VM" || virsh_cmd destroy "$VM" ;;
console)  cmd_console ;;
ssh)      cmd_ssh ;;
baseline) cmd_baseline ;;
destroy)  cmd_destroy ;;
purge)    cmd_purge ;;
*)
	sed -n '2,25p' "$0"
	exit 2
	;;
esac
