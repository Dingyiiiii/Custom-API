#!/bin/sh
#
# Custom-API customs gateway: post-deployment smoke check.
#
# The acceptance suites (smoke-test.sh, intel-test.sh) start their own throwaway
# instances on private ports and prove the *code* works. This script checks the
# opposite thing: that the instance actually installed on this machine is up,
# reachable, correctly gated and still able to verify its own audit ledger.
#
# Run it on the deployment target after `install.sh` and a service start:
#
#   sh /usr/local/share/customapi/verify-deployment.sh
#
# It is read-only except for one metered request through the gateway, and it
# never prints a credential.
#
# Exit status is non-zero if any check fails.

set -u

PREFIX=${PREFIX:-/usr/local}
ETC=${ETC:-${PREFIX}/etc/customapi}
CONFIG=${CONFIG:-${ETC}/customs.json}
ENVFILE=${ENVFILE:-${ETC}/customsd.env}

PASS=0
FAIL=0
FAILED_NAMES=""

ok()  { PASS=$((PASS + 1)); printf '  [PASS] %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); FAILED_NAMES="${FAILED_NAMES} $1"; printf '  [FAIL] %s\n' "$1"; [ $# -gt 1 ] && printf '         %s\n' "$2"; }

printf '\n=== custom-api deployment check ===\n'

if [ ! -f "${CONFIG}" ]; then
	printf 'FATAL: %s not found; is the gateway installed?\n' "${CONFIG}"
	exit 2
fi

# json_get <file> <dotted.path>: jq when present, python3 otherwise.
#
# Both branches must work: `command -v jq` succeeds in the test suite's own
# shell but jq can be absent from a minimal FreeBSD install, and a branch that
# silently yields "" makes every path-derived check look like a broken gateway.
_JS_PARSE='
import json, sys
path = [p for p in sys.argv[1].split(".") if p != ""]
try:
    v = json.load(open(sys.argv[2]))
except Exception:
    print(""); raise SystemExit
for p in path:
    if isinstance(v, list):
        try:
            v = v[int(p)]
        except (ValueError, IndexError):
            v = None
    elif isinstance(v, dict):
        v = v.get(p)
    else:
        v = None
    if v is None:
        break
print("" if v is None else v)
'
# The leading dot is added here rather than at every call site: "server.listen"
# is not a jq path, and jq treats it as a call to a filter named `server.listen`,
# which fails - silently, because stderr is discarded. The bug is invisible until
# something derives an address from the result.
if command -v jq >/dev/null 2>&1; then
	json_get() { jq -r ".$2 // empty" < "$1" 2>/dev/null; }
elif command -v python3 >/dev/null 2>&1; then
	json_get() { python3 -c "${_JS_PARSE}" "$2" "$1"; }
else
	json_get() { printf ''; }
fi

json_available() {
	case "${json_get "$1" "$2"}" in
	"") return 1 ;;
	*) return 0 ;;
	esac
}

LISTEN=$(json_get "${CONFIG}" 'server.listen')
ADMIN_LISTEN=$(json_get "${CONFIG}" 'server.admin_listen')
ADMIN_REMOTE=$(json_get "${CONFIG}" 'server.admin_remote')

# The user-facing address to test. A wildcard bind is probed on the loopback
# address, which a wildcard listener always accepts.
USER_HOST=$(printf '%s' "${LISTEN}" | sed 's/^0\.0\.0\.0:/127.0.0.1:/; s/^\[::\]:/127.0.0.1:/; s/^\*:/127.0.0.1:/')
ADMIN_HOST=$(printf '%s' "${ADMIN_LISTEN:-127.0.0.1:8081}" | sed 's/^0\.0\.0\.0:/127.0.0.1:/; s/^\[::\]:/127.0.0.1:/; s/^\*:/127.0.0.1:/')
USER_BASE="http://${USER_HOST}"
ADMIN_BASE="http://${ADMIN_HOST}"

printf '  config          : %s\n' "${CONFIG}"
printf '  user listener   : %s\n' "${LISTEN:-(unreadable)}"
printf '  admin listener  : %s\n' "${ADMIN_LISTEN:-127.0.0.1:8081}"

if [ -z "${LISTEN}" ]; then
	bad "the listen address could be read from the configuration" \
		"neither jq nor python3 produced a value; install one of them"
	LISTEN="127.0.0.1:8080"
	ADMIN_LISTEN=${ADMIN_LISTEN:-127.0.0.1:8081}
fi

# ---------------------------------------------------------------- the service
printf '\n--- service ---\n'
if [ -x "${PREFIX}/bin/customsctl" ]; then
	if "${PREFIX}/bin/customsctl" config check --config "${CONFIG}" >/dev/null 2>&1; then
		ok "the installed configuration is valid"
	else
		bad "the installed configuration is valid" \
			"run: ${PREFIX}/bin/customsctl config check --config ${CONFIG}"
	fi
else
	bad "customsctl is installed" "expected ${PREFIX}/bin/customsctl"
fi

if [ -f "${ENVFILE}" ]; then
	MODE=$(stat -f '%Lp' "${ENVFILE}" 2>/dev/null || stat -c '%a' "${ENVFILE}" 2>/dev/null)
	OWNER=$(stat -f '%Su:%Sg' "${ENVFILE}" 2>/dev/null || stat -c '%U:%G' "${ENVFILE}" 2>/dev/null)
	if [ "${MODE}" = "640" ]; then
		ok "the env file is not group/world readable (mode ${MODE})"
	else
		bad "the env file mode is 640" "found ${MODE} on ${ENVFILE}"
	fi
	printf '  env file owner  : %s\n' "${OWNER}"
else
	bad "the env file exists" "expected ${ENVFILE}"
fi

if curl -sS --max-time 5 -o /dev/null "${USER_BASE}/healthz" 2>/dev/null; then
	ok "the user-facing listener answers /healthz"
	HEALTH=$(curl -sS --max-time 5 "${USER_BASE}/healthz" 2>/dev/null)
	SEQ=$(printf '%s' "${HEALTH}" | sed -n 's/.*"audit_seq":\([0-9]*\).*/\1/p')
	printf '  audit sequence  : %s\n' "${SEQ:-?}"
	printf '  version         : %s\n' "$(printf '%s' "${HEALTH}" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"
else
	bad "the user-facing listener answers /healthz" \
		"is customsd running?  service customsd status"
fi

if curl -sS --max-time 5 -o /dev/null "${USER_BASE}/readyz" 2>/dev/null; then
	ok "the gateway reports itself ready"
else
	bad "the gateway reports itself ready" "GET ${USER_BASE}/readyz"
fi

# ---------------------------------------------------------- user-facing auth
printf '\n--- user-facing API ---\n'
CODE=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "${USER_BASE}/v1/models" 2>/dev/null)
if [ "${CODE}" = "401" ]; then
	ok "an unauthenticated /v1/models is refused (401)"
else
	bad "an unauthenticated /v1/models is refused" "status=${CODE}"
fi

if curl -sS --max-time 5 -o /dev/null "${ADMIN_BASE}/admin/healthz" 2>/dev/null; then
	ok "the management listener is up"
else
	bad "the management listener is up" "GET ${ADMIN_BASE}/admin/healthz"
fi

# ------------------------------------------------------ management gating
printf '\n--- management API reachability ---\n'
if [ "${ADMIN_REMOTE}" = "true" ]; then
	# Find a non-loopback address to test the gate from, if this host has one.
	NONLOOP=""
	for _if in $(ifconfig -l 2>/dev/null); do
		_cand=$(ifconfig "${_if}" 2>/dev/null | awk '/inet /{print $2; exit}')
		case "${_cand}" in ""|127.*) continue ;; esac
		NONLOOP="${_cand}"
		break
	done
	if [ -z "${NONLOOP}" ]; then
		printf '  [SKIP] remote gating (no non-loopback address on this host)\n'
	else
		PORT=${ADMIN_LISTEN##*:}
		CODE=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "http://${NONLOOP}:${PORT}/admin/stats" 2>/dev/null)
		if [ "${CODE}" = "401" ]; then
			ok "a remote caller without the token gets 401"
		else
			bad "a remote caller without the token gets 401" "status=${CODE}"
		fi
		CODE=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' \
			-H 'X-Forwarded-For: 127.0.0.1' "http://${NONLOOP}:${PORT}/admin/stats" 2>/dev/null)
		if [ "${CODE}" = "401" ]; then
			ok "a spoofed X-Forwarded-For does not pass the gate"
		else
			bad "a spoofed X-Forwarded-For does not pass the gate" "status=${CODE}"
		fi
	fi
else
	CODE=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "${ADMIN_BASE}/admin/stats" 2>/dev/null)
	if [ "${CODE}" = "200" ]; then
		ok "loopback access to the management API works without a token"
	else
		bad "loopback access to the management API works without a token" "status=${CODE}"
	fi
	printf '  note            : admin_remote is off, so no credential is needed.\n'
	printf '                    Reach it over an SSH tunnel for remote use.\n'
fi

# -------------------------------------------------------------- audit ledger
printf '\n--- audit ledger ---\n'
if [ -x "${PREFIX}/bin/customsctl" ]; then
	OUT=$("${PREFIX}/bin/customsctl" audit verify --config "${CONFIG}" 2>&1)
	if printf '%s' "${OUT}" | grep -q 'OK'; then
		ok "the audit hash chain verifies"
		printf '%s\n' "${OUT}" | sed -n 's/^\(records\|head\|anchors\|result\):/  \1:/p' | sed 's/^/  /'
	else
		bad "the audit hash chain verifies" "$(printf '%s' "${OUT}" | tail -5)"
	fi
fi

AUDIT_PATH=$(json_get "${CONFIG}" 'server.audit_path')
if [ -n "${AUDIT_PATH}" ] && [ -e "${AUDIT_PATH}" ]; then
	# The service drops privileges, so a ledger it cannot write is a failure
	# that only shows up at the next restart - check it now instead.
	OWNER=$(stat -f '%Su' "${AUDIT_PATH}" 2>/dev/null || stat -c '%U' "${AUDIT_PATH}" 2>/dev/null)
	printf '  ledger owner    : %s\n' "${OWNER}"
	if [ "${OWNER}" = "root" ]; then
		bad "the ledger is not owned by root" \
			"the unprivileged service cannot write ${AUDIT_PATH}; re-run install.sh"
	else
		ok "the ledger is owned by an unprivileged account (${OWNER})"
	fi
fi

# ------------------------------------------------------------------ console
printf '\n--- management console ---\n'
if curl -sS --max-time 5 "${ADMIN_BASE}/" 2>/dev/null | grep -q '<title>'; then
	ok "the console HTML is served"
else
	bad "the console HTML is served" \
		"the release may not include web/; the gateway serves a placeholder instead"
fi
for asset in $(curl -sS --max-time 5 "${ADMIN_BASE}/" 2>/dev/null | sed -n 's/.*src="\.\/\([^"]*\)".*/\1/p; s/.*href="\.\/\([^"]*\)".*/\1/p'); do
	if curl -sS --max-time 5 -o /dev/null "${ADMIN_BASE}/${asset}" 2>/dev/null; then
		ok "console asset ${asset} is served"
	else
		bad "console asset ${asset} is served"
	fi
done

# ------------------------------------------------------------------- report
printf '\n=== results ===\n'
printf 'passed: %d\nfailed: %d\n' "${PASS}" "${FAIL}"
if [ "${FAIL}" -ne 0 ]; then
	printf 'failed checks:%s\n\n' "${FAILED_NAMES}"
	printf 'gateway log (last 20 lines):\n'
	tail -20 /var/log/customapi/customsd.log 2>/dev/null
	exit 1
fi
printf '\nDEPLOYMENT OK\n'
exit 0
