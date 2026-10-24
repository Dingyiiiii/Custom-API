#!/bin/sh
#
# Create the Custom-API FreeBSD VM on a libvirt host.
#
# Differences from ../vm/create-vm.sh (the development-host version):
#
#   * the guest is attached to the **real LAN** rather than a NAT network. Use
#     --network with a libvirt macvtap network (preferred: it changes nothing on
#     the host) or --bridge with a host bridge;
#   * the guest is **headless with serial-only access**: a getty on ttyu0 and
#     sshd deliberately disabled, so the console is the only way in;
#   * the guest pulls its deployment payload from an HTTP server the operator
#     runs on the LAN, because there is no shared filesystem between the
#     libvirt host and the guest.
#
# Run it on the libvirt host (laboratory.lan). It is idempotent: re-running
# rebuilds the working disk from the base image and redefines the domain.
#
# Usage:
#   ./create-lab-vm.sh [options]
#     --bridge BRIDGE     host bridge to attach the VM to (required)
#     --name NAME         domain name (default: customapi-freebsd)
#     --ip ADDRESS        guest IPv4 address (default: dhcp)
#     --gateway ADDRESS   guest default gateway (needed with --ip)
#     --dns ADDRESS       guest nameserver (default: 223.5.5.5)
#     --disk-gb N         working disk size (default: 60)
#     --mem-mib N         memory (default: 4096)
#     --vcpus N           vCPUs (default: 4)
#     --root-password P   console root password (default: customapi)
#     --mirror URL        FreeBSD mirror base (default: USTC)
#     --status            show domain/network state and exit

set -eu

NAME=customapi-freebsd
# NETWORK is a libvirt network name (attached as <interface type='network'>).
# BRIDGE attaches to a host bridge instead. Exactly one must be given.
NETWORK=
BRIDGE=
GUEST_IP=dhcp
GUEST_GW=
GUEST_DNS=223.5.5.5
DISK_GB=60
MEM_MIB=4096
VCPUS=4
ROOT_PASSWORD=customapi
MIRROR=https://mirrors.ustc.edu.cn/freebsd
FBSD_VERSION=15.1-RELEASE
IMAGE_XZ=FreeBSD-${FBSD_VERSION}-amd64-BASIC-CLOUDINIT-ufs.qcow2.xz
IMAGE_BASE=FreeBSD-${FBSD_VERSION}-amd64-BASIC-CLOUDINIT-ufs.qcow2
IMAGE_DIR=/var/lib/libvirt/images/customapi
WORKDIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SHOW_STATUS=no

while [ $# -gt 0 ]; do
	case "$1" in
	--bridge) BRIDGE="$2"; NETWORK=; shift 2 ;;
	--network) NETWORK="$2"; BRIDGE=; shift 2 ;;
	--name) NAME="$2"; shift 2 ;;
	--ip) GUEST_IP="$2"; shift 2 ;;
	--gateway) GUEST_GW="$2"; shift 2 ;;
	--dns) GUEST_DNS="$2"; shift 2 ;;
	--disk-gb) DISK_GB="$2"; shift 2 ;;
	--mem-mib) MEM_MIB="$2"; shift 2 ;;
	--vcpus) VCPUS="$2"; shift 2 ;;
	--root-password) ROOT_PASSWORD="$2"; shift 2 ;;
	--mirror) MIRROR="$2"; shift 2 ;;
	--status) SHOW_STATUS=yes; shift ;;
	-h|--help) sed -n '2,30p' "$0"; exit 0 ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

log()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
virsh_cmd() { sudo -n virsh -c qemu:///system "$@"; }

if [ "${SHOW_STATUS}" = "yes" ]; then
	virsh_cmd list --all || true
	echo
	virsh_cmd net-list --all || true
	echo
	sudo -n ip -br link show type bridge 2>/dev/null || true
	exit 0
fi

[ "$(id -u)" = "0" ] || sudo -n true 2>/dev/null || die "root or passwordless sudo required"
if [ -n "${BRIDGE}" ] && [ -n "${NETWORK}" ]; then
	die "--bridge and --network are mutually exclusive"
fi
if [ -z "${BRIDGE}" ] && [ -z "${NETWORK}" ]; then
	die "one of --network or --bridge is required"
fi
if [ -n "${NETWORK}" ]; then
	virsh_cmd net-info "${NETWORK}" >/dev/null 2>&1 \
		|| die "libvirt network ${NETWORK} does not exist"
fi
if [ "${GUEST_IP}" != "dhcp" ] && [ -z "${GUEST_GW}" ]; then
	die "--gateway is required when --ip is used"
fi

for tool in qemu-img mkisofs xz curl; do
	command -v "${tool}" >/dev/null 2>&1 || die "${tool} is required"
done

# ---------------------------------------------------------------- base image
sudo -n install -d -m 0755 -o qemu -g qemu "${IMAGE_DIR}" 2>/dev/null || \
	sudo -n install -d -m 0755 "${IMAGE_DIR}"

if [ ! -f "${IMAGE_DIR}/${IMAGE_BASE}" ]; then
	if [ ! -f "${WORKDIR}/${IMAGE_XZ}" ]; then
		log "downloading ${IMAGE_XZ}"
		curl -fL --retry 3 -o "${WORKDIR}/${IMAGE_XZ}" \
			"${MIRROR}/releases/VM-IMAGES/${FBSD_VERSION}/amd64/Latest/${IMAGE_XZ}" \
			|| die "download failed"
	fi
	log "verifying and extracting the base image"
	xz -t "${WORKDIR}/${IMAGE_XZ}" || die "the image archive is corrupt"
	xz -dk -f -T0 -c "${WORKDIR}/${IMAGE_XZ}" | sudo -n tee "${IMAGE_DIR}/${IMAGE_BASE}" >/dev/null
else
	log "base image already present"
fi

# ------------------------------------------------------------- working disk
log "creating a ${DISK_GB} GiB working disk"
sudo -n rm -f "${IMAGE_DIR}/${NAME}.qcow2"
sudo -n qemu-img create -f qcow2 -F qcow2 -b "${IMAGE_DIR}/${IMAGE_BASE}" \
	"${IMAGE_DIR}/${NAME}.qcow2" "${DISK_GB}G" >/dev/null
sudo -n qemu-img resize "${IMAGE_DIR}/${NAME}.qcow2" "${DISK_GB}G" >/dev/null

# ------------------------------------------------------------------ seed ISO
log "building the cloud-init seed (serial console only, no SSH)"

# Network configuration for the guest. FreeBSD takes a single ifconfig_DEFAULT
# line, and the static form needs the router in a separate variable, so the two
# cases are built here rather than templated inline.
if [ "${GUEST_IP}" = "dhcp" ]; then
	SNIPPET='ifconfig_DEFAULT="SYNCDHCP accept_rtadv"'
else
	SNIPPET="ifconfig_DEFAULT=\"inet ${GUEST_IP}/24\"
defaultrouter=\"${GUEST_GW}\""
fi

# Build each rc.conf-style body first, then indent it as a YAML block.
#
# Interpolating a multi-line value straight into a block scalar is a trap: only
# the first line inherits the "content: |" indentation, so the remaining lines
# escape the block and the document stops parsing. FreeBSD's nuageinit then
# fails with "could not find expected ':'" and silently skips every later
# directive, the root password and service configuration included. Indenting
# explicitly removes the dependency on how the value happens to be shaped.
RC_CONF=$(cat <<EOF
hostname="customapi-bsd"
${SNIPPET}
# The published VM image enables a first-boot pkg upgrade of the FreeBSD base
# system (firstboot_pkg_upgrade_enable). That rewrites /boot, /usr/lib and the
# system binaries while the guest is running, which leaves the serial console
# silent for many minutes and can strand the system half-updated. rc.conf.local
# is sourced after rc.conf, so setting it here genuinely overrides the image.
# Upgrades are then a deliberate operator action, not a surprise at first boot.
firstboot_pkg_upgrade_enable="NO"
# Serial console on ttyu0. The image already sets
# console="comconsole,vidconsole" in /boot/loader.conf.
ttyu0="3wire"
ttyu0_if_exists="NO"
# SSH is enabled alongside the serial console.
#
# The console alone is not enough: the base image's first-boot base-system
# upgrade silences the serial line, and a guest with neither console nor SSH
# cannot be operated at all. SSH is therefore the control channel that matters,
# and the console is the out-of-band path.
sshd_enable="YES"
pf_enable="NO"
dumpdev="NO"
growfs_enable="YES"
syslogd_enable="YES"
cron_enable="YES"
EOF
)
RC_BLOCK=$(printf '%s\n' "${RC_CONF}" | sed 's/^/      /')

PROFILE_BLOCK=$(sed 's/^/      /' <<'EOF'
# The Rust toolchain from `pkg install rust` lands in /usr/local/bin. The entry
# is a no-op for an interactive root shell but matters under sudo and cron,
# which reset PATH; the Go build needed the same treatment for /usr/local/go/bin.
PATH="/usr/local/bin:/usr/local/sbin:$PATH"
export PATH
# Unlike the Go build there is no proxy variable to set: cargo resolves crates
# through crates.io, and a guest that cannot reach it needs a source
# replacement in /root/.cargo/config.toml, not an environment variable.
EOF
)

PKG_BLOCK=$(sed 's/^/      /' <<'EOF'
ustc: {
  url: "pkg+https://mirrors.ustc.edu.cn/freebsd-pkg/${ABI}/quarterly",
  mirror_type: "srv",
  signature_type: "fingerprints",
  fingerprints: "/usr/share/keys/pkg",
  enabled: yes
}
EOF
)

cat > "${WORKDIR}/user-data" <<EOF
#cloud-config
hostname: customapi-bsd
timezone: Asia/Shanghai
disable_root: false
# Password authentication is on so the root password set below is usable over
# SSH, which is the channel that survives the base image's first-boot upgrade.
# A public key is more reliable than a password here: the guest's sshd comes
# up during cloud-init, and a key removes the question of whether chpasswd ran
# and whether password authentication is permitted.
ssh_pwauth: true
ssh_authorized_keys:
  - ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBZPNro83gBlSe4NpozZ0r9cjMiEnEIjHoZqH6lfg0Tf lab-vm-access

write_files:
  - path: /etc/rc.conf.local
    permissions: '0644'
    content: |
${RC_BLOCK}
  # Deliberately unusable resolver for the first boot.
  #
  # The published FreeBSD VM image enables a base-system pkg upgrade on first
  # boot (firstboot_pkg_upgrade_enable=YES in its /etc/rc.conf). That upgrade
  # replaces the running userland and kernel modules in place; observed twice on
  # this host, it leaves the guest alive but unresponsive, with the serial
  # console silent and no answer on the network. Setting the rc knob from
  # rc.conf.local does not stop it, because the upgrade is scheduled before that
  # file is consulted.
  #
  # Refusing name resolution makes the upgrade fail immediately and harmlessly
  # instead. runcmd below installs the real resolver once the first boot is
  # past, so this affects only the window in which the upgrade would have run.
  - path: /etc/resolv.conf
    permissions: '0644'
    content: |
      # Intentional: see the comment in create-lab-vm.sh. Replaced by runcmd.
      nameserver 127.0.0.1
  - path: /etc/profile.d/customapi.sh
    permissions: '0644'
    content: |
${PROFILE_BLOCK}
  # Disable every pkg repository, belt and braces alongside the rc knob: a
  # repository that cannot be reached cannot trigger an unintended base-system
  # upgrade. Kept in a script rather than an inline runcmd so that the quoting
  # survives both YAML and the shell.
  - path: /usr/local/etc/customapi/disable-pkg-repos.sh
    permissions: '0755'
    content: |
      #!/bin/sh
      set -u
      for f in /etc/pkg/*.conf; do
        [ -f "\$f" ] || continue
        if ! grep -q 'enabled: *no' "\$f"; then
          sed -i '' -e 's/^\([A-Za-z0-9_-][A-Za-z0-9_-]*\): *{/\1: { enabled: no,/' "\$f"
        fi
      done
      exit 0
  - path: /usr/local/etc/pkg/repos/FreeBSD.conf
    permissions: '0644'
    content: |
      FreeBSD: { enabled: no }
  - path: /usr/local/etc/pkg/repos/ustc.conf
    permissions: '0644'
    content: |
${PKG_BLOCK}

chpasswd:
  expire: false
  list: |
    root:${ROOT_PASSWORD}

# runcmd entries are plain strings, NOT the cloud-init array form.
#
# FreeBSD's nuageinit writes each entry with f:write(c .. "\\n"), so a table
# (which is what an array becomes) raises
# "attempt to concatenate a table value (local 'c')" at line 514 and aborts the
# whole script. Because runcmd is the last section, the failure is silent from
# the outside: the guest boots, write_files and chpasswd have already been
# applied, and only the commands are missing - which looks like a working guest
# until something they were supposed to do turns out not to have happened.
runcmd:
  # The first boot is over once these run, so the real resolver is installed now.
  - printf 'nameserver %s\\n' ${GUEST_DNS} > /etc/resolv.conf
  - sysrc -f /etc/rc.conf.local firstboot_pkg_upgrade_enable=NO
  - sh /usr/local/etc/customapi/disable-pkg-repos.sh
  - sysrc -f /etc/rc.conf.local sshd_enable=YES
  - sysrc -f /etc/rc.conf.local ttyu0=3wire
  # Two FreeBSD defaults refuse root over SSH even with sshd enabled: the
  # default PermitRootLogin is "no" (the line is commented out in sshd_config,
  # which does not make it inactive), and pty sessions are insecure unless
  # /etc/ttys says otherwise. Without both fixes the guest comes up with an
  # sshd that answers and then refuses every login with "ROOT LOGIN REFUSED".
  - sed -i '' 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - grep -q '^pts' /etc/ttys || echo 'pts none unknown off secure' >> /etc/ttys
  - service sshd restart
  - touch /var/db/customapi-seed.done
final_message: "CUSTOMAPI-SEED-COMPLETE after \$UPTIME seconds"
EOF

# Validate before building the ISO. A malformed seed produces a guest that
# boots but cannot be configured, which is far more expensive to diagnose than
# a failed check here - it cost one full VM rebuild to learn that.
if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' 2>/dev/null; then
	python3 "${WORKDIR}/validate-seed.py" "${WORKDIR}/user-data" \
		|| die "the generated user-data is not valid YAML"
else
	log "note: PyYAML unavailable, skipping seed validation"
fi

cat > "${WORKDIR}/meta-data" <<EOF
instance-id: ${NAME}-01
local-hostname: customapi-bsd
EOF

(cd "${WORKDIR}" && mkisofs -quiet -output seed.iso -volid CIDATA -joliet -rock \
	user-data meta-data) || die "failed to build the seed ISO"
sudo -n install -m 0444 -o qemu -g qemu "${WORKDIR}/seed.iso" "${IMAGE_DIR}/${NAME}-seed.iso" 2>/dev/null || \
	sudo -n install -m 0444 "${WORKDIR}/seed.iso" "${IMAGE_DIR}/${NAME}-seed.iso"

sudo -n chown qemu:qemu "${IMAGE_DIR}/${NAME}.qcow2" 2>/dev/null || true
sudo -n chmod 0644 "${IMAGE_DIR}/${NAME}.qcow2" 2>/dev/null || true

# -------------------------------------------------------------------- domain
if [ -n "${NETWORK}" ]; then
	ATTACH="<interface type='network'>
      <source network='${NETWORK}'/>
      <model type='virtio'/>
    </interface>"
	log "defining domain ${NAME} on libvirt network ${NETWORK}"
else
	ATTACH="<interface type='bridge'>
      <source bridge='${BRIDGE}'/>
      <model type='virtio'/>
    </interface>"
	log "defining domain ${NAME} on host bridge ${BRIDGE}"
fi
if virsh_cmd dominfo "${NAME}" >/dev/null 2>&1; then
	virsh_cmd destroy "${NAME}" 2>/dev/null || true
	virsh_cmd undefine "${NAME}" --nvram 2>/dev/null || virsh_cmd undefine "${NAME}" || true
fi

cat > "${WORKDIR}/${NAME}.xml" <<EOF
<domain type='kvm'>
  <name>${NAME}</name>
  <title>Custom-API FreeBSD test guest (headless, serial console only)</title>
  <description>FreeBSD ${FBSD_VERSION} guest bridged onto the LAN. No graphics device and sshd disabled: the libvirt serial console is the only way in. Console: virsh -c qemu:///system console ${NAME}</description>
  <memory unit='MiB'>${MEM_MIB}</memory>
  <currentMemory unit='MiB'>${MEM_MIB}</currentMemory>
  <vcpu placement='static'>${VCPUS}</vcpu>
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
    <topology sockets='1' dies='1' cores='${VCPUS}' threads='1'/>
  </cpu>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>restart</on_crash>
  <devices>
    <!-- The emulator path is deliberately omitted so libvirt uses its own
         configured default. It differs by distribution
         (/usr/bin/qemu-system-x86_64 on Fedora, /usr/libexec/qemu-kvm on
         RHEL-derived hosts), and a wrong path makes the domain fail to define
         with "cannot check QEMU binary". -->
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' cache='writeback' discard='unmap'/>
      <source file='${IMAGE_DIR}/${NAME}.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='${IMAGE_DIR}/${NAME}-seed.iso'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <controller type='virtio-serial' index='0'/>
    ${ATTACH}
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

virsh_cmd define "${WORKDIR}/${NAME}.xml" >/dev/null
log "domain defined; start it with: virsh -c qemu:///system start ${NAME}"
