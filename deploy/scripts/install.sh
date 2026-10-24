#!/bin/sh
#
# Custom-API customs gateway: FreeBSD installation script.
#
# Run as root on the target host AFTER copying the release directory to
# /tmp/customapi-release. It is idempotent: re-running it upgrades the binaries
# and configuration templates without touching the audit ledger, the quota state
# or the installed secrets file.
#
#	sh install.sh [--prefix /usr/local] [--force-config] [--uninstall]
#
set -eu

PREFIX=/usr/local

# Resolve the release root. The staged layout is
#   <release>/scripts/install.sh, <release>/bin/*, <release>/rc.d/*, <release>/etc/*
# so the payload normally lives one level above this script. A flat layout
# (everything beside the script) is also accepted.
SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -d "${SELF_DIR}/../bin" ]; then
	SRC_DIR=$(CDPATH= cd -- "${SELF_DIR}/.." && pwd)
else
	SRC_DIR=${SELF_DIR}
fi
ETC_DIR="${PREFIX}/etc/customapi"
DB_DIR=/var/db/customapi
LOG_DIR=/var/log/customapi
SERVICE_USER=customapi
UNINSTALL=no
FORCE_CONFIG=no

while [ $# -gt 0 ]; do
	case "$1" in
	--prefix) PREFIX="$2"; shift 2 ;;
	--uninstall) UNINSTALL=yes; shift ;;
	--force-config) FORCE_CONFIG=yes; shift ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done
ETC_DIR="${PREFIX}/etc/customapi"

if [ "$(id -u)" != "0" ]; then
	echo "install.sh: must be run as root" >&2
	exit 1
fi

if [ "${UNINSTALL}" = "yes" ]; then
	echo "--- stopping services"
	service customsd onestop 2>/dev/null || true
	service benchd onestop 2>/dev/null || true
	sysrc -x customsd_enable 2>/dev/null || true
	sysrc -x benchd_enable 2>/dev/null || true
	echo "--- removing binaries"
	rm -f "${PREFIX}/sbin/customsd" "${PREFIX}/sbin/benchd" "${PREFIX}/bin/customsctl"
	rm -f "${PREFIX}/libexec/customapi/customsd-wrapper"
	rm -rf "${PREFIX}/share/customapi/web"
	rm -f "${PREFIX}/etc/rc.d/customsd" "${PREFIX}/etc/rc.d/benchd"
	echo "--- preserving ${DB_DIR} (audit ledger), ${ETC_DIR}, ${LOG_DIR}"
	echo "uninstalled. Remove those directories manually to purge data."
	exit 0
fi

echo "=== Custom-API customs gateway installer ==="

# ------------------------------------------------------------------ user
if ! pw usershow "${SERVICE_USER}" >/dev/null 2>&1; then
	echo "--- creating service user ${SERVICE_USER}"
	pw useradd "${SERVICE_USER}" -c "Custom-API customs gateway" \
		-d /var/db/customapi -s /usr/sbin/nologin -m
else
	echo "--- service user ${SERVICE_USER} already exists"
fi

# ------------------------------------------------------------- directories
echo "--- creating directories"
install -d -m 0755 "${ETC_DIR}"
install -d -o "${SERVICE_USER}" -g "${SERVICE_USER}" -m 0750 "${DB_DIR}"
install -d -o "${SERVICE_USER}" -g "${SERVICE_USER}" -m 0750 "${LOG_DIR}"
# benchd writes the price catalogue, benchmark reports and scorecard here.
install -d -o "${SERVICE_USER}" -g "${SERVICE_USER}" -m 0750 "${DB_DIR}/state"
install -d -m 0755 "${PREFIX}/sbin" "${PREFIX}/bin" "${PREFIX}/etc/rc.d"

# --------------------------------------------------------------- binaries
echo "--- installing binaries"
for b in customsd benchd customsctl; do
	if [ ! -f "${SRC_DIR}/bin/${b}" ]; then
		echo "install.sh: missing ${SRC_DIR}/bin/${b}" >&2
		echo "install.sh: the release directory looks incomplete; expected bin/, etc/, rc.d/, scripts/" >&2
		exit 1
	fi
done
for b in customsd benchd; do
	install -m 0755 -o root -g wheel "${SRC_DIR}/bin/${b}" "${PREFIX}/sbin/${b}"
done
install -m 0755 -o root -g wheel "${SRC_DIR}/bin/customsctl" "${PREFIX}/bin/customsctl"

# Privilege-dropping launcher used by the customsd rc.d script.
install -d -m 0755 "${PREFIX}/libexec/customapi"
install -m 0755 -o root -g wheel "${SRC_DIR}/scripts/customsd-wrapper.sh" \
	"${PREFIX}/libexec/customapi/customsd-wrapper"

# The acceptance test and its mock upstream are shipped so that an operator can
# re-verify a deployment at any time, without the development tree.
if [ -d "${PREFIX}/share/customapi" ] || mkdir -p "${PREFIX}/share/customapi" 2>/dev/null; then
	install -m 0755 -o root -g wheel "${SRC_DIR}/scripts/smoke-test.sh" \
		"${PREFIX}/share/customapi/smoke-test.sh" 2>/dev/null || true
	install -m 0755 -o root -g wheel "${SRC_DIR}/scripts/mock-upstream.py" \
		"${PREFIX}/share/customapi/mock-upstream.py" 2>/dev/null || true
	install -m 0755 -o root -g wheel "${SRC_DIR}/scripts/intel-test.sh" \
		"${PREFIX}/share/customapi/intel-test.sh" 2>/dev/null || true
	install -m 0755 -o root -g wheel "${SRC_DIR}/scripts/verify-deployment.sh" \
		"${PREFIX}/share/customapi/verify-deployment.sh" 2>/dev/null || true
fi

# The management UI. It is installed to a fixed location that the gateway
# searches by default, so the operator does not have to set CUSTOMS_WEB_DIR.
if [ -f "${SRC_DIR}/web/index.html" ]; then
	echo "--- installing the management UI"
	install -d -m 0755 "${PREFIX}/share/customapi/web"
	# Replace rather than merge: a stale hashed asset from a previous release
	# would otherwise linger forever.
	rm -rf "${PREFIX}/share/customapi/web"/*
	cp -R "${SRC_DIR}/web/." "${PREFIX}/share/customapi/web/"
	chmod -R a+rX "${PREFIX}/share/customapi/web"
	echo "    ${PREFIX}/share/customapi/web"
else
	echo "--- no web UI in this release; the gateway will serve a placeholder"
fi
echo "    ${PREFIX}/sbin/customsd"
echo "    ${PREFIX}/sbin/benchd"
echo "    ${PREFIX}/bin/customsctl"

# ---------------------------------------------------------------- rc.d
echo "--- installing rc.d scripts"
for s in customsd benchd; do
	install -m 0755 -o root -g wheel "${SRC_DIR}/rc.d/${s}" "${PREFIX}/etc/rc.d/${s}"
done

# ----------------------------------------------------------- configuration
# An installed configuration holds the operator's own policy, so it is never
# replaced silently. The new template is written alongside for comparison, and
# --force-config installs it over the top for the case where the operator wants
# the upstream template (for example after an upgrade changed its shape).
if [ "${FORCE_CONFIG}" = "yes" ] && [ -f "${ETC_DIR}/customs.json" ]; then
	backup="${ETC_DIR}/customs.json.bak.$(date +%Y%m%d%H%M%S)"
	echo "--- --force-config: backing up ${ETC_DIR}/customs.json to ${backup}"
	cp -p "${ETC_DIR}/customs.json" "${backup}"
	install -m 0644 -o root -g wheel "${SRC_DIR}/etc/customs.json" "${ETC_DIR}/customs.json"
	echo "--- installed the template over ${ETC_DIR}/customs.json"
elif [ -f "${ETC_DIR}/customs.json" ]; then
	echo "--- keeping existing ${ETC_DIR}/customs.json"
	install -m 0644 -o root -g wheel "${SRC_DIR}/etc/customs.json" \
		"${ETC_DIR}/customs.json.new"
	echo "    template written to ${ETC_DIR}/customs.json.new for comparison"
else
	echo "--- installing ${ETC_DIR}/customs.json"
	install -m 0644 -o root -g wheel "${SRC_DIR}/etc/customs.json" "${ETC_DIR}/customs.json"
fi

if [ -f "${ETC_DIR}/customsd.env" ]; then
	echo "--- keeping existing ${ETC_DIR}/customsd.env"
	# Repair the ownership/mode: an upgrade from an earlier release may have
	# left it readable only by root, which breaks the service after a restart.
	chown root:"${SERVICE_USER}" "${ETC_DIR}/customsd.env"
	chmod 0640 "${ETC_DIR}/customsd.env"
else
	echo "--- creating ${ETC_DIR}/customsd.env from the template"
	echo "    ACTION REQUIRED: replace every 'replace-me' value, then start the service"
	# Group-readable by the service user: the daemon reads this file after
	# dropping privileges, so root-only would make the service unrunnable.
	install -m 0640 -o root -g "${SERVICE_USER}" \
		"${SRC_DIR}/etc/customsd.env.example" "${ETC_DIR}/customsd.env"
fi

# ------------------------------------------------------------- state files
# The ledger and anchors are written by the service at runtime, but they are
# also rewritten by operator tooling (customsctl audit rehash) that is normally
# run as root. A rebuild replaces the file through a rename, so a ledger that
# belonged to the service can end up owned by root, and the next service start
# fails with "permission denied" on its own evidence store. Reconcile ownership
# on every install so an upgrade repairs that state instead of inheriting it.
echo "--- repairing ownership of ${DB_DIR}"
chown "${SERVICE_USER}":"${SERVICE_USER}" "${DB_DIR}" 2>/dev/null || true
chmod 0750 "${DB_DIR}" 2>/dev/null || true
for f in "${DB_DIR}/audit.jsonl" "${DB_DIR}/anchor.jsonl" "${DB_DIR}/catalogue.json"; do
	if [ -e "$f" ]; then
		chown "${SERVICE_USER}":"${SERVICE_USER}" "$f"
		chmod 0640 "$f"
		echo "    $f"
	fi
done

# ------------------------------------------------------------- rc.conf
echo "--- enabling services in /etc/rc.conf"
sysrc customsd_enable=YES
sysrc customsd_config="${ETC_DIR}/customs.json"
sysrc customsd_envfile="${ETC_DIR}/customsd.env"
sysrc customsd_service_user="${SERVICE_USER}"
sysrc benchd_enable=YES
sysrc benchd_config="${ETC_DIR}/customs.json"
sysrc benchd_envfile="${ETC_DIR}/customsd.env"
sysrc benchd_service_user="${SERVICE_USER}"
sysrc benchd_gateway="http://127.0.0.1:8080"

# ------------------------------------------------------- newsyslog rules
if [ -d /etc/newsyslog.conf.d ]; then
	cat > /etc/newsyslog.conf.d/customapi.conf <<'NEWSYSLOG'
# Custom-API customs gateway logs
/var/log/customapi/customsd.log  customapi:customapi  640  7  1000  *  JC
/var/log/customapi/benchd.log    customapi:customapi  640  7  1000  *  JC
NEWSYSLOG
	echo "--- installed /etc/newsyslog.conf.d/customapi.conf"
fi

echo
echo "=== installation complete ==="
echo
echo "Next steps:"
echo "  1. Edit ${ETC_DIR}/customsd.env and replace every placeholder token."
echo "     Generate tokens with: openssl rand -hex 32"
echo "  2. Review ${ETC_DIR}/customs.json (channels, users, policy, prices)."
echo "  3. Validate the configuration:"
echo "       ${PREFIX}/bin/customsctl config check --config ${ETC_DIR}/customs.json"
echo "  4. Start the gateway:  service customsd start"
echo "  5. Verify the audit chain: ${PREFIX}/bin/customsctl audit verify --config ${ETC_DIR}/customs.json"
echo "  6. Start the intelligence daemon: service benchd start"
echo
echo "The audit ledger lives in ${DB_DIR}/audit.jsonl and is append-only."
echo "Back it up; it is the system's evidence of record."
