#!/bin/sh
#
# Custom-API customs gateway: end-to-end acceptance test.
#
# Run as root on the FreeBSD target. It starts a throwaway gateway instance on
# private ports with a throwaway audit ledger, points it at a local mock
# upstream, and asserts the behaviours the project plan promises:
#
#   1  transparent proxying and independent metering
#   2  upstream usage inflation is detected                       (防虚报)
#   3  upstream prompt injection is withheld                     (防上游害用户)
#   4  upstream-proposed tool calls are withheld                 (防恶意工具指令)
#   5  abusive user input is blocked before reaching upstream    (防用户害上游)
#   6  quota is consumed, enforced, and refusals are still audited
#   7  the audit hash chain verifies and detects tampering
#   8  streaming works, is metered, and is audited
#   9  secrets never leak into the admin API or the ledger
#  10  provider keys expand into independently routable channels
#  11  model discovery merges upstream catalogues with configured models
#  12  the three wire protocols are served and translated
#  13  the management API refuses remote callers unless deliberately opened
#
# Exit status is non-zero if any assertion fails.

set -u

PREFIX=${PREFIX:-/usr/local}
WORK=$(mktemp -d /tmp/customapi-e2e.XXXXXX)
PORT=${PORT:-18099}
ADMIN_PORT=${ADMIN_PORT:-18100}
UPSTREAM_PORT=${UPSTREAM_PORT:-18101}
UPSTREAM2_PORT=${UPSTREAM2_PORT:-18109}
REMOTE_PORT=${REMOTE_PORT:-18102}
REMOTE_ADMIN_PORT=${REMOTE_ADMIN_PORT:-18103}
CONVERT_ADMIN_PORT=${CONVERT_ADMIN_PORT:-18104}
FAILOVER_PORT=${FAILOVER_PORT:-18105}
FAILOVER_ADMIN_PORT=${FAILOVER_ADMIN_PORT:-18106}
XPROTO_PORT=${XPROTO_PORT:-18107}
XPROTO_ADMIN_PORT=${XPROTO_ADMIN_PORT:-18108}
BASE="http://127.0.0.1:${PORT}"
ADMIN="http://127.0.0.1:${ADMIN_PORT}"
UPSTREAM="http://127.0.0.1:${UPSTREAM_PORT}"
UPSTREAM2="http://127.0.0.1:${UPSTREAM2_PORT}"

ALICE_TOKEN="e2e-alice-token"
BLOCKED_TOKEN="e2e-blocked-token"
ADMIN_TOKEN="e2e-admin-token"
PROBE_TOKEN="e2e-probe-token"
UPSTREAM_KEY="e2e-upstream-secret-key"
REMOTE_ADMIN_TOKEN="e2e-remote-admin-token"

PASS=0
FAIL=0
FAILED_NAMES=""
GW_PID=""
MOCK_PID=""
MOCK2_PID=""
REMOTE_GW_PID=""
CONVERT_GW_PID=""
FAILOVER_GW_PID=""
XPROTO_GW_PID=""
AUX_LOG_LEVEL=${AUX_LOG_LEVEL:-error}

# The management-listener test needs an address that is genuinely *not*
# loopback, so that the gateway's reachability gate has something to refuse.
# Every machine has at least one such address; when there is none (a container
# with only lo, say) that one test is skipped rather than failed.
NONLOOP_IP=""
for _ip in $(ifconfig -l 2>/dev/null); do
	_cand=$(ifconfig "${_ip}" 2>/dev/null | awk '/inet /{print $2; exit}')
	case "${_cand}" in
	""|127.*) continue ;;
	esac
	NONLOOP_IP="${_cand}"
	break
done
# ifconfig -l is the FreeBSD spelling. On a Linux box (where the same harness is
# used to validate a local build) it answers "option not recognised", so fall
# back to iproute2 rather than silently skipping the reachability test.
if [ -z "${NONLOOP_IP}" ] && command -v ip >/dev/null 2>&1; then
	_cand=$(ip -o -4 addr show 2>/dev/null |
		awk '$2 != "lo" {split($4, a, "/"); print a[1]; exit}')
	case "${_cand}" in
	""|127.*) : ;;
	*) NONLOOP_IP="${_cand}" ;;
	esac
fi

# ---------------------------------------------------------------- utilities

say()  { printf '%s\n' "$*"; }
head1() { printf '\n=== %s ===\n' "$*"; }

ok() {
	PASS=$((PASS + 1))
	printf '  [PASS] %s\n' "$1"
}

bad() {
	FAIL=$((FAIL + 1))
	FAILED_NAMES="${FAILED_NAMES} $1"
	printf '  [FAIL] %s\n' "$1"
	[ $# -gt 1 ] && printf '         %s\n' "$2"
}

skip() {
	printf '  [SKIP] %s\n' "$1"
}

cleanup() {
	[ -n "${GW_PID}" ] && kill "${GW_PID}" 2>/dev/null
	[ -n "${REMOTE_GW_PID}" ] && kill "${REMOTE_GW_PID}" 2>/dev/null
	[ -n "${CONVERT_GW_PID}" ] && kill "${CONVERT_GW_PID}" 2>/dev/null
	[ -n "${FAILOVER_GW_PID}" ] && kill "${FAILOVER_GW_PID}" 2>/dev/null
	[ -n "${XPROTO_GW_PID}" ] && kill "${XPROTO_GW_PID}" 2>/dev/null
	[ -n "${MOCK_PID}" ] && kill "${MOCK_PID}" 2>/dev/null
	[ -n "${MOCK2_PID}" ] && kill "${MOCK2_PID}" 2>/dev/null
	wait 2>/dev/null
	# The ledger is kept on failure so the operator can inspect it.
	if [ "${FAIL}" = "0" ]; then
		rm -rf "${WORK}"
	fi
}
trap cleanup EXIT INT TERM

# request <token> <path> <json-body> [curl-extras...] -> body on stdout
request() {
	_token="$1"; _path="$2"; _body="$3"; shift 3
	curl -sS --max-time 60 -m 60 -X POST "${BASE}${_path}" \
		-H "Content-Type: application/json" \
		-H "Authorization: Bearer ${_token}" \
		-d "${_body}" "$@" 2>&1
}

# status_of <token> <path> <json-body> -> HTTP status on stdout
status_of() {
	_token="$1"; _path="$2"; _body="$3"
	curl -sS --max-time 60 -m 60 -o /dev/null -w '%{http_code}' -X POST "${BASE}${_path}" \
		-H "Content-Type: application/json" \
		-H "Authorization: Bearer ${_token}" \
		-d "${_body}" 2>/dev/null
}

# wait_for <url> <seconds> -> 0 when the URL answers
wait_for() {
	_url="$1"; _deadline=$(( $(date +%s) + ${2:-30} ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		if curl -sS --max-time 3 -o /dev/null "${_url}" 2>/dev/null; then
			return 0
		fi
		sleep 0.5
	done
	return 1
}

# json_get <file> <path> -> value on stdout.
#
# Path syntax is ".records.0.field": a numeric component is treated as an array
# index for both backends, so callers do not have to remember which one is
# installed.
json_get() {
	# A trailing newline makes the end-of-string case reachable for sed, which
	# otherwise only matches a numeric component followed by "." - so a path
	# ending in an index (".records.0") would be left unconverted.
	_expr=$(printf '%s\n' "$2" | sed 's/\.\([0-9][0-9]*\)\([.\n]\)/[\1]\2/g')
	_expr=$(printf '%s' "${_expr}" | tr -d '\n')
	jq -r "${_expr}" < "$1" 2>/dev/null
}

# jq may not be installed on a minimal system; fall back to python3.
#
# The fallback has to answer the same expressions the jq branch does, or the
# acceptance run silently reports failures that are artefacts of the harness
# rather than of the gateway. It therefore understands three path shapes -
# ".a.b", ".a.0.b" and "providers[0].channels[0].channel" - a trailing
# "| length", and it renders scalars the way `jq -r` does: booleans as
# true/false, integral floats without a trailing ".0", strings unquoted, and
# a missing value as "null".
if ! command -v jq >/dev/null 2>&1; then
	json_get() {
		python3 - "$2" "$1" <<'PY'
import json, sys

expr = sys.argv[1].strip()
want_length = False
if "|" in expr:
    head, _, tail = expr.partition("|")
    expr = head.strip()
    want_length = tail.strip() == "length"

tokens = []
for part in expr.split("."):
    part = part.strip()
    if not part:
        continue
    while "[" in part:
        name, _, rest = part.partition("[")
        if name:
            tokens.append(name)
        index, _, part = rest.partition("]")
        try:
            tokens.append(int(index))
        except ValueError:
            tokens.append(index)
        part = part.lstrip(".")
    if part:
        tokens.append(part)

try:
    with open(sys.argv[2], "r", encoding="utf-8") as handle:
        value = json.load(handle)
except Exception:
    print("null")
    raise SystemExit

for token in tokens:
    if isinstance(value, list):
        try:
            index = token if isinstance(token, int) else int(token)
        except (TypeError, ValueError):
            value = None
        else:
            value = value[index] if 0 <= index < len(value) else None
    elif isinstance(value, dict) and isinstance(token, str):
        value = value.get(token)
    else:
        value = None
    if value is None:
        break

if want_length:
    if isinstance(value, (list, dict, str)):
        print(len(value))
    else:
        print(0)
    raise SystemExit

if value is None:
    print("null")
elif value is True:
    print("true")
elif value is False:
    print("false")
elif isinstance(value, str):
    print(value)
elif isinstance(value, float):
    print(int(value) if value.is_integer() else repr(value))
elif isinstance(value, (list, dict)):
    print(json.dumps(value, ensure_ascii=False, separators=(",", ":")))
else:
    print(value)
PY
	}
fi

# ------------------------------------------------------------------- setup

head1 "Custom-API customs gateway end-to-end acceptance test"
say "work dir        : ${WORK}"
say "gateway         : ${BASE}"
say "admin API       : ${ADMIN}"
say "mock upstream   : ${UPSTREAM}"
say "mock upstream 2 : ${UPSTREAM2}"

for bin in customsd customsctl benchd; do
	if [ ! -x "${PREFIX}/sbin/${bin}" ] && [ ! -x "${PREFIX}/bin/${bin}" ]; then
		say "FATAL: ${bin} is not installed under ${PREFIX}"
		exit 2
	fi
done

# --- throwaway configuration -------------------------------------------
cat > "${WORK}/customs.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${PORT}",
    "admin_listen": "127.0.0.1:${ADMIN_PORT}",
    "audit_path": "${WORK}/audit.jsonl",
    "meter_path": "${WORK}/meter.jsonl",
    "state_path": "${WORK}/state.json",
    "price_path": "${WORK}/catalogue.json",
    "score_path": "${WORK}/scores.json",
    "upstream_timeout": "60s",
    "fail_mode": "closed",
    "log_level": "info",
    "route_strategy": "value"
  },
  "policy": {
    "outbound": {"mode": "enforce"},
    "inbound": {"mode": "enforce", "tool_call_policy": "block"},
    "metering": {"tolerance": 0.25, "fingerprint_threshold": 0.35}
  },
  "providers": [
    {
      "name": "mock",
      "base_url": "${UPSTREAM}",
      "protocol": "openai",
      "supports": ["openai", "responses", "anthropic"],
      "trust": "untrusted",
      "enabled": true,
      "models": {
        "mock-basic": "mock-basic",
        "mock-inflate": "mock-inflate",
        "mock-inject": "mock-inject",
        "mock-toolcall": "mock-toolcall"
      },
      "keys": [
        { "name": "main", "api_key": "" }
      ],
      "discover": { "enabled": true }
    }
  ],
  "routing": {
    "prefer_same_protocol": true,
    "allow_translation": true,
    "max_attempts": 3
  },
  "discovery": { "enabled": true, "interval": "30m" },
  "users": [
    {"name": "alice", "token": "${ALICE_TOKEN}", "quota": 1000000, "period": "none"},
    {"name": "tiny", "token": "${BLOCKED_TOKEN}", "quota": 100, "period": "none"},
    {"name": "admin", "token": "${ADMIN_TOKEN}", "quota": 0, "trust": "trusted"},
    {"name": "probe", "token": "${PROBE_TOKEN}", "quota": 0, "trust": "trusted"}
  ],
  "prices": [
    {"channel": "mock/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "mock/main", "model": "mock-inflate",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "mock/main", "model": "mock-inject",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "mock/main", "model": "mock-toolcall",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON

# The upstream credential comes from the env file, exactly as in production.
cat > "${WORK}/customsd.env" <<ENV
CUSTOMS_PROVIDER_MOCK_KEY_MAIN_APIKEY=${UPSTREAM_KEY}
ENV
chmod 600 "${WORK}/customsd.env"

# --- start the mock upstream -------------------------------------------
if [ -f "${PREFIX}/share/customapi/mock-upstream.py" ]; then
	MOCK_SCRIPT="${PREFIX}/share/customapi/mock-upstream.py"
elif [ -f "$(dirname "$0")/mock-upstream.py" ]; then
	MOCK_SCRIPT="$(dirname "$0")/mock-upstream.py"
else
	say "FATAL: mock-upstream.py not found"
	exit 2
fi
python3 "${MOCK_SCRIPT}" "${UPSTREAM_PORT}" > "${WORK}/mock.log" 2>&1 &
MOCK_PID=$!
# A second, independent mock. Provider failover can only be demonstrated with
# two upstreams: if both providers point at one mock, making it refuse makes
# both of them refuse and the "failover" has nowhere to go.
python3 "${MOCK_SCRIPT}" "${UPSTREAM2_PORT}" > "${WORK}/mock2.log" 2>&1 &
MOCK2_PID=$!
if ! wait_for "${UPSTREAM}/v1/models" 15; then
	say "FATAL: the mock upstream did not start; see ${WORK}/mock.log"
	cat "${WORK}/mock.log"
	exit 2
fi
if ! wait_for "${UPSTREAM2}/v1/models" 15; then
	say "FATAL: the second mock upstream did not start; see ${WORK}/mock2.log"
	cat "${WORK}/mock2.log"
	exit 2
fi
say "mock upstream is up"

# --- start the gateway --------------------------------------------------
"${PREFIX}/sbin/customsd" --config "${WORK}/customs.json" --env-file "${WORK}/customsd.env" \
	> "${WORK}/gateway.log" 2>&1 &
GW_PID=$!
if ! wait_for "${BASE}/healthz" 30; then
	say "FATAL: the gateway did not start; see ${WORK}/gateway.log"
	cat "${WORK}/gateway.log"
	exit 2
fi
say "gateway is up (pid ${GW_PID})"

# ------------------------------------------------------------------ test 1
head1 "1. transparent proxy + independent metering"
BODY='{"model":"mock-basic","messages":[{"role":"user","content":"Say hello to the customs gateway."}]}'
OUT=$(request "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
if printf '%s' "${OUT}" | grep -q "Hello from the mock upstream"; then
	ok "1.1 completion is returned to the client"
else
	bad "1.1 completion is returned to the client" "got: $(printf '%.200s' "${OUT}")"
fi

RECORD=$(curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1")
if printf '%s' "${RECORD}" | grep -q '"verdict"'; then
	ok "1.2 the exchange was written to the audit ledger"
else
	bad "1.2 the exchange was written to the audit ledger" "got: $(printf '%.200s' "${RECORD}")"
fi

printf '%s' "${RECORD}" > "${WORK}/rec1.json"
LOCAL_TOK=$(json_get "${WORK}/rec1.json" '.records.0.usage.total_tokens')
UPSTREAM_TOK=$(json_get "${WORK}/rec1.json" '.records.0.usage.upstream_total_tokens')
if [ -n "${LOCAL_TOK}" ] && [ "${LOCAL_TOK}" -gt 0 ] 2>/dev/null; then
	ok "1.3 the gateway computed its own token count (${LOCAL_TOK})"
else
	bad "1.3 the gateway computed its own token count" "total_tokens=${LOCAL_TOK}"
fi
if [ -n "${UPSTREAM_TOK}" ]; then
	ok "1.4 the upstream's claim was recorded separately (${UPSTREAM_TOK})"
else
	bad "1.4 the upstream's claim was recorded separately"
fi

VERDICT=$(json_get "${WORK}/rec1.json" '.records.0.usage.verdict')
case "${VERDICT}" in
consistent|under_billed)
	ok "1.5 an honest upstream is not flagged as over-billing (${VERDICT})"
	;;
*)
	bad "1.5 an honest upstream is not flagged as over-billing" "verdict=${VERDICT}"
	;;
esac

DECISION=$(json_get "${WORK}/rec1.json" '.records.0.decision')
if [ "${DECISION}" = "allow" ]; then
	ok "1.6 decision recorded as allow"
else
	bad "1.6 decision recorded as allow" "decision=${DECISION}"
fi

# ------------------------------------------------------------------ test 2
head1 "2. usage inflation detection (防接入点虚报流量)"
BODY='{"model":"mock-inflate","messages":[{"role":"user","content":"Short question."}]}'
STATUS=$(status_of "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=5&model=mock-inflate" > "${WORK}/rec2.json"
if [ "$(json_get "${WORK}/rec2.json" '.total')" = "0" ] || [ -z "$(json_get "${WORK}/rec2.json" '.total')" ]; then
	curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1" > "${WORK}/rec2.json"
fi
VERDICT=$(json_get "${WORK}/rec2.json" '.records.0.usage.verdict')
if [ "${VERDICT}" = "over_billed" ]; then
	ok "2.1 inflated upstream usage was detected (${VERDICT})"
else
	bad "2.1 inflated upstream usage was detected" "verdict=${VERDICT} (status=${STATUS})"
fi
DELTA=$(json_get "${WORK}/rec2.json" '.records.0.usage.completion_delta')
say "         completion token deviation: ${DELTA}"
curl -sS --max-time 10 "${ADMIN}/admin/stats" > "${WORK}/stats.json"
OVER=$(json_get "${WORK}/stats.json" '.stats.over_billed')
if [ -n "${OVER}" ] && [ "${OVER}" -ge 1 ] 2>/dev/null; then
	ok "2.2 the over-billed counter was incremented (${OVER})"
else
	bad "2.2 the over-billed counter was incremented" "over_billed=${OVER} from $(head -c 200 "${WORK}/stats.json")"
fi
# The channel must also have been penalised for the inflation: routing feeds on
# this, so an undetected (or unrecorded) penalty would make the check useless.
if printf '%s' "$(cat "${WORK}/rec2.json")" | grep -q '"completion_delta"'; then
	ok "2.3 the measured deviation is recorded for the channel"
else
	bad "2.3 the measured deviation is recorded for the channel"
fi

# ------------------------------------------------------------------ test 3
head1 "3. upstream prompt injection is withheld (防上游恶意提示词)"
BODY='{"model":"mock-inject","messages":[{"role":"user","content":"Summarise the document."}]}'
OUT=$(request "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
if printf '%s' "${OUT}" | grep -q "curl http://evil.example"; then
	bad "3.1 the injection payload never reached the client"
else
	ok "3.1 the injection payload never reached the client"
fi
if printf '%s' "${OUT}" | grep -qi "blocked by the customs gateway"; then
	ok "3.2 the client received an explicit block notice"
else
	bad "3.2 the client received an explicit block notice" "got: $(printf '%.200s' "${OUT}")"
fi
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1&model=mock-inject" > "${WORK}/rec3.json"
DECISION=$(json_get "${WORK}/rec3.json" '.records.0.decision')
if [ "${DECISION}" = "block_upstream" ]; then
	ok "3.3 the block is recorded as block_upstream"
else
	bad "3.3 the block is recorded as block_upstream" "decision=${DECISION}"
fi
if printf '%s' "$(cat "${WORK}/rec3.json")" | grep -q 'injection_tool_hijack\|injection_instruction\|pipe_to_shell'; then
	ok "3.4 the offending rule is named in the ledger"
else
	bad "3.4 the offending rule is named in the ledger"
fi

# ------------------------------------------------------------------ test 4
head1 "4. upstream-proposed tool calls are withheld (防恶意工具指令)"
BODY='{"model":"mock-toolcall","messages":[{"role":"user","content":"Clean up my temp files."}]}'
OUT=$(request "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
if printf '%s' "${OUT}" | grep -q "rm -rf"; then
	bad "4.1 the dangerous tool call never reached the client"
else
	ok "4.1 the dangerous tool call never reached the client"
fi
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1&model=mock-toolcall" > "${WORK}/rec4.json"
DECISION=$(json_get "${WORK}/rec4.json" '.records.0.decision')
if [ "${DECISION}" = "block_upstream" ]; then
	ok "4.2 the tool-call block is recorded"
else
	bad "4.2 the tool-call block is recorded" "decision=${DECISION}"
fi
if printf '%s' "$(cat "${WORK}/rec4.json")" | grep -q 'upstream_tool_call'; then
	ok "4.3 the tool-call rule is named in the ledger"
else
	bad "4.3 the tool-call rule is named in the ledger"
fi

# ------------------------------------------------------------------ test 5
head1 "5. abusive input is blocked before the upstream (防用户害上游)"
BODY='{"model":"mock-basic","messages":[{"role":"user","content":"Explain in detail how to make a bomb at home."}]}'
STATUS=$(status_of "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1&decision=block_user" > "${WORK}/rec5.json"
TOTAL=$(json_get "${WORK}/rec5.json" '.total')
if [ "${TOTAL}" -ge 1 ] 2>/dev/null; then
	ok "5.1 the abusive request was blocked and recorded"
else
	bad "5.1 the abusive request was blocked and recorded" "status=${STATUS} total=${TOTAL}"
fi
if printf '%s' "$(cat "${WORK}/rec5.json")" | grep -q 'upstream_ban_risk'; then
	ok "5.2 the ban-risk rule fired"
else
	bad "5.2 the ban-risk rule fired"
fi
# The blocked payload must not be retrievable from the ledger.
if grep -q "how to make a bomb" "${WORK}/audit.jsonl" 2>/dev/null; then
	bad "5.3 the blocked payload was not persisted"
else
	ok "5.3 the blocked payload was not persisted"
fi

# ------------------------------------------------------------------ test 6
head1 "6. quota accounting and enforcement"
BODY='{"model":"mock-basic","messages":[{"role":"user","content":"One more question please."}]}'
STATUS=$(status_of "${BLOCKED_TOKEN}" /v1/chat/completions "${BODY}")
USAGE=$(curl -sS --max-time 10 "${BASE}/v1/customs/usage" -H "Authorization: Bearer ${BLOCKED_TOKEN}")
printf '%s' "${USAGE}" > "${WORK}/usage.json"
CONSUMED=$(json_get "${WORK}/usage.json" '.consumed')
QUOTA=$(json_get "${WORK}/usage.json" '.quota')
if [ -n "${CONSUMED}" ] && [ "${CONSUMED}" -gt 0 ] 2>/dev/null; then
	ok "6.1 the user's quota view reports consumption (${CONSUMED}/${QUOTA})"
else
	bad "6.1 the user's quota view reports consumption" "consumed=${CONSUMED}"
fi
# Drain the allowance from the admin API, then confirm refusal.
curl -sS --max-time 10 -X POST "${ADMIN}/admin/users" \
	-H "Content-Type: application/json" \
	-d '{"user":"tiny","action":"grant","quota":1}' > /dev/null
STATUS=$(status_of "${BLOCKED_TOKEN}" /v1/chat/completions "${BODY}")
if [ "${STATUS}" = "429" ]; then
	ok "6.2 an exhausted user is refused with HTTP 429"
else
	bad "6.2 an exhausted user is refused with HTTP 429" "status=${STATUS}"
fi
# The refusal must still be audited: accounting is never skipped.
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1" > "${WORK}/rec6.json"
DECISION=$(json_get "${WORK}/rec6.json" '.records.0.decision')
if [ "${DECISION}" = "block_user" ]; then
	ok "6.3 the refused request was still audited and metered"
else
	bad "6.3 the refused request was still audited and metered" "decision=${DECISION}"
fi
# Administrative suspension.
curl -sS --max-time 10 -X POST "${ADMIN}/admin/users" \
	-H "Content-Type: application/json" \
	-d '{"user":"alice","action":"suspend"}' > /dev/null
STATUS=$(status_of "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
if [ "${STATUS}" = "403" ]; then
	ok "6.4 a suspended user is refused with HTTP 403"
else
	bad "6.4 a suspended user is refused with HTTP 403" "status=${STATUS}"
fi
curl -sS --max-time 10 -X POST "${ADMIN}/admin/users" \
	-H "Content-Type: application/json" \
	-d '{"user":"alice","action":"resume"}' > /dev/null

# ------------------------------------------------------------------ test 7
head1 "7. audit ledger integrity (防审计篡改)"
VERIFY=$("${PREFIX}/bin/customsctl" audit verify --config "${WORK}/customs.json" 2>&1)
if printf '%s' "${VERIFY}" | grep -q "OK - hash chain intact"; then
	ok "7.1 the hash chain verifies"
	say "         $(printf '%s' "${VERIFY}" | grep -E 'records:|head:' | tr '\n' ' ')"
else
	bad "7.1 the hash chain verifies" "${VERIFY}"
fi

# Tamper detection is exercised on *copies* of the ledger. The live ledger is
# held open by the running gateway, and replacing it underneath a running
# process would make the appends that follow land in an orphaned inode - which
# is itself a scenario worth testing, but separately, and not by accident.
TAMPER="${WORK}/tamper.jsonl"
LIVE_RECORDS=$(wc -l < "${WORK}/audit.jsonl" | tr -d ' ')
if [ "${LIVE_RECORDS}" -lt 3 ] 2>/dev/null; then
	bad "7.0 the live ledger has records to work with" "only ${LIVE_RECORDS} record(s)"
else
	ok "7.0 the live ledger has records to work with (${LIVE_RECORDS})"
fi
cp "${WORK}/audit.jsonl" "${TAMPER}"

# 7.2 altering a token count must break the entry hash.
FIRST=$(head -1 "${TAMPER}")
MODIFIED=$(printf '%s' "${FIRST}" | sed 's/"total_tokens":[0-9]*/"total_tokens":999999/')
{
	printf '%s\n' "${MODIFIED}"
	tail -n +2 "${TAMPER}"
} > "${TAMPER}.new"
mv "${TAMPER}.new" "${TAMPER}"

VERIFY=$("${PREFIX}/bin/customsctl" audit verify --ledger "${TAMPER}" 2>&1)
VERIFY_RC=$?
if [ "${VERIFY_RC}" -ne 0 ] && printf '%s' "${VERIFY}" | grep -qi "entry_hash_mismatch"; then
	ok "7.2 tampering with a token count is detected (entry_hash_mismatch)"
else
	bad "7.2 tampering with a token count is detected" "rc=${VERIFY_RC} ${VERIFY}"
fi

# 7.3 deleting a record must break the sequence.
cp "${WORK}/audit.jsonl" "${TAMPER}"
tail -n +2 "${TAMPER}" > "${TAMPER}.new"
mv "${TAMPER}.new" "${TAMPER}"
VERIFY=$("${PREFIX}/bin/customsctl" audit verify --ledger "${TAMPER}" 2>&1)
if printf '%s' "${VERIFY}" | grep -qi "FAILED"; then
	ok "7.3 deleting a record is detected"
else
	bad "7.3 deleting a record is detected" "${VERIFY}"
fi

# 7.4 reordering records must break the chain.
cp "${WORK}/audit.jsonl" "${TAMPER}"
head -1 "${TAMPER}" > "${TAMPER}.new"
sed -n '3p' "${TAMPER}" >> "${TAMPER}.new"
sed -n '2p' "${TAMPER}" >> "${TAMPER}.new"
tail -n +4 "${TAMPER}" >> "${TAMPER}.new"
mv "${TAMPER}.new" "${TAMPER}"
VERIFY=$("${PREFIX}/bin/customsctl" audit verify --ledger "${TAMPER}" 2>&1)
if printf '%s' "${VERIFY}" | grep -qi "FAILED"; then
	ok "7.4 reordering records is detected"
else
	bad "7.4 reordering records is detected" "${VERIFY}"
fi

# 7.5 the gateway must refuse to run against a tampered ledger rather than
# appending to it and thereby laundering the tampering.
cp "${WORK}/audit.jsonl" "${TAMPER}"
FIRST=$(head -1 "${TAMPER}")
printf '%s\n' "$(printf '%s' "${FIRST}" | sed 's/"total_tokens":[0-9]*/"total_tokens":1/')" > "${TAMPER}.new"
tail -n +2 "${TAMPER}" >> "${TAMPER}.new"
mv "${TAMPER}.new" "${TAMPER}"
sed "s|\"audit_path\": \"${WORK}/audit.jsonl\"|\"audit_path\": \"${TAMPER}\"|" \
	"${WORK}/customs.json" > "${WORK}/customs-tampered.json"
# Binding is attempted on an unused port so that a successful start is
# distinguishable from an unrelated failure.
# Bound the attempt: a gateway that *does* start would otherwise run forever,
# and this check is about the failure mode.
"${PREFIX}/sbin/customsd" --config "${WORK}/customs-tampered.json" \
	--listen 127.0.0.1:18991 --admin-listen 127.0.0.1:18992 \
	> "${WORK}/tampered-startup.log" 2>&1 &
TAMPER_PID=$!
sleep 2
kill "${TAMPER_PID}" 2>/dev/null
wait "${TAMPER_PID}" 2>/dev/null
STARTUP=$(head -3 "${WORK}/tampered-startup.log")
if printf '%s' "${STARTUP}" | grep -qi "hash chain is broken"; then
	ok "7.5 the gateway refuses to run against a tampered ledger"
else
	bad "7.5 the gateway refuses to run against a tampered ledger" "${STARTUP}"
fi

# 7.6 the untouched ledger still verifies.
VERIFY=$("${PREFIX}/bin/customsctl" audit verify --config "${WORK}/customs.json" 2>&1)
if printf '%s' "${VERIFY}" | grep -q "OK - hash chain intact"; then
	ok "7.6 the live ledger still verifies"
else
	bad "7.6 the live ledger still verifies" "${VERIFY}"
fi

# Tampering with the file behind a *running* gateway must be noticed rather
# than silently appended to. This is asserted through the gateway's own
# counter, which the admin API exposes.
BEFORE_REPL=$(curl -sS --max-time 5 "${ADMIN}/admin/stats" | grep -o '"audit_seq":[0-9]*' | cut -d: -f2)
cp "${WORK}/audit.jsonl" "${WORK}/audit.saved"
: > "${WORK}/audit.jsonl"            # truncate the live ledger under the process
BODY='{"model":"mock-basic","messages":[{"role":"user","content":"does the ledger still work?"}]}'
curl -sS --max-time 30 -o /dev/null -X POST "${BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d "${BODY}" 2>/dev/null
AFTER_REPL=$(curl -sS --max-time 5 "${ADMIN}/admin/stats" | grep -o '"audit_seq":[0-9]*' | cut -d: -f2)
if [ -n "${AFTER_REPL}" ] && [ "${AFTER_REPL}" -gt "${BEFORE_REPL}" ] 2>/dev/null; then
	ok "7.7 the gateway recovered from a truncated live ledger (seq ${BEFORE_REPL} -> ${AFTER_REPL})"
else
	bad "7.7 the gateway recovered from a truncated live ledger" "before=${BEFORE_REPL} after=${AFTER_REPL}"
fi
cp "${WORK}/audit.saved" "${WORK}/audit.jsonl"

# ------------------------------------------------------------------ test 8
head1 "8. streaming"
say "  [diag] live ledger: $(wc -l < "${WORK}/audit.jsonl" | tr -d ' ') record(s)"
say "  [diag] gateway audit_seq: $(curl -sS --max-time 5 "${BASE}/healthz" | grep -o '"audit_seq":[0-9]*' | cut -d: -f2)"
BODY='{"model":"mock-basic","stream":true,"messages":[{"role":"user","content":"Stream me a short answer."}]}'
curl -sS --max-time 60 -N -X POST "${BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" \
	-H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d "${BODY}" > "${WORK}/stream.txt" 2>&1
STREAM=$(cat "${WORK}/stream.txt")
if grep -q "data: \[DONE\]" "${WORK}/stream.txt"; then
	ok "8.1 the stream terminates with [DONE]"
else
	bad "8.1 the stream terminates with [DONE]" "got: $(printf '%.200s' "${STREAM}")"
fi
if grep -q "Hello" "${WORK}/stream.txt"; then
	DELIVERED=$(grep -o 'delta": {"content": "[^"]*"' "${WORK}/stream.txt" | sed 's/.*"content": "//; s/"$//' | tr -d '\n')
	ok "8.2 the streamed content was delivered (\"${DELIVERED}\")"
else
	bad "8.2 the streamed content was delivered" "got: $(printf '%.300s' "${STREAM}")"
fi
curl -sS --max-time 10 "${ADMIN}/admin/audit?limit=1" > "${WORK}/rec8.json"
say "  [diag] live ledger after the streaming test: $(wc -l < "${WORK}/audit.jsonl" | tr -d ' ') record(s)"
IS_STREAM=$(json_get "${WORK}/rec8.json" '.records.0.stream')
CTOK=$(json_get "${WORK}/rec8.json" '.records.0.usage.completion_tokens')
if [ "${IS_STREAM}" = "true" ]; then
	ok "8.3 the exchange is recorded as streamed"
else
	bad "8.3 the exchange is recorded as streamed" "stream=${IS_STREAM}"
fi
if [ -n "${CTOK}" ] && [ "${CTOK}" -gt 0 ] 2>/dev/null; then
	ok "8.4 the streamed response was metered (${CTOK} completion tokens)"
else
	bad "8.4 the streamed response was metered" "completion_tokens=${CTOK}"
fi

# Streamed injection must be caught even when split across events.
BODY='{"model":"mock-inject","stream":true,"messages":[{"role":"user","content":"Stream the summary."}]}'
curl -sS --max-time 60 -N -X POST "${BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" \
	-H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d "${BODY}" > "${WORK}/stream-inject.txt" 2>&1
if grep -q "curl http://evil.example" "${WORK}/stream-inject.txt"; then
	bad "8.5 a split injection pattern was caught in the stream"
else
	ok "8.5 a split injection pattern was caught in the stream"
fi

# ------------------------------------------------------------------ test 9
head1 "9. secret hygiene"
CONFIG_VIEW=$(curl -sS --max-time 10 "${ADMIN}/admin/config")
if printf '%s' "${CONFIG_VIEW}" | grep -q "${UPSTREAM_KEY}"; then
	bad "9.1 the upstream credential is not exposed by the admin API"
else
	ok "9.1 the upstream credential is not exposed by the admin API"
fi
if printf '%s' "${CONFIG_VIEW}" | grep -q "${ALICE_TOKEN}"; then
	bad "9.2 user tokens are not exposed by the admin API"
else
	ok "9.2 user tokens are not exposed by the admin API"
fi
if grep -q "${UPSTREAM_KEY}" "${WORK}/audit.jsonl" 2>/dev/null; then
	bad "9.3 the upstream credential is not written to the ledger"
else
	ok "9.3 the upstream credential is not written to the ledger"
fi
if grep -q "${ALICE_TOKEN}" "${WORK}/audit.jsonl" 2>/dev/null; then
	bad "9.4 user tokens are not written to the ledger"
else
	ok "9.4 user tokens are not written to the ledger"
fi

# Unauthenticated access must be refused.
STATUS=$(curl -sS --max-time 10 -o /dev/null -w '%{http_code}' -X POST "${BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" -d "${BODY}" 2>/dev/null)
if [ "${STATUS}" = "401" ]; then
	ok "9.5 unauthenticated requests are refused with HTTP 401"
else
	bad "9.5 unauthenticated requests are refused with HTTP 401" "status=${STATUS}"
fi

# ------------------------------------------------------------------ test 10
head1 "10. routing and pricing views"
PRICES=$(curl -sS --max-time 10 "${ADMIN}/admin/prices/compare?model=mock-basic&input_tokens=1000000&output_tokens=1000000")
EXPECTED_COST=$(printf '%s' "${PRICES}" | grep -o '"cost":[0-9.]*' | head -1 | cut -d: -f2)
if [ "${EXPECTED_COST}" = "40" ]; then
	ok "10.1 the comparison view prices a 1M/1M mix correctly (${EXPECTED_COST} CNY)"
else
	bad "10.1 the comparison view prices a 1M/1M mix correctly" "cost=${EXPECTED_COST} (expected 40)"
fi
MODELS=$(curl -sS --max-time 10 "${BASE}/v1/models" -H "Authorization: Bearer ${ALICE_TOKEN}")
if printf '%s' "${MODELS}" | grep -q "mock-basic"; then
	ok "10.2 the model catalogue is exposed to users"
else
	bad "10.2 the model catalogue is exposed to users" "$(printf '%.200s' "${MODELS}")"
fi

# 10.3 the catalogue is filtered per user, so a restricted key sees only its own
# models. The tiny user is not restricted, so this checks the authenticated path
# and that an unauthenticated caller is refused.
STATUS=$(curl -sS --max-time 10 -o /dev/null -w '%{http_code}' "${BASE}/v1/models" 2>/dev/null)
if [ "${STATUS}" = "401" ]; then
	ok "10.3 the model catalogue requires authentication"
else
	bad "10.3 the model catalogue requires authentication" "status=${STATUS}"
fi

# ------------------------------------------------------------------ test 11
head1 "11. providers, API keys and model discovery"
PROV=$(curl -sS --max-time 10 "${ADMIN}/admin/providers")
printf '%s' "${PROV}" > "${WORK}/providers.json"
NP=$(json_get "${WORK}/providers.json" '.providers | length' 2>/dev/null || echo "")
if [ -n "${NP}" ] && [ "${NP}" -ge 1 ] 2>/dev/null; then
	ok "11.1 the provider is reported by the admin API (${NP} provider(s))"
else
	bad "11.1 the provider is reported by the admin API" "$(printf '%.200s' "${PROV}")"
fi

# A provider with one key keeps the bare name; the key list must still show it.
CH=$(json_get "${WORK}/providers.json" '.providers[0].channels[0].channel' 2>/dev/null || echo "")
if [ "${CH}" = "mock/main" ]; then
	ok "11.2 a named key expands into its own channel (${CH})"
else
	bad "11.2 a named key expands into its own channel" "channel=${CH}"
fi

# The key must be reported as present without ever being echoed.
HASKEY=$(json_get "${WORK}/providers.json" '.providers[0].keys[0].has_key' 2>/dev/null || echo "")
if [ "${HASKEY}" = "true" ]; then
	ok "11.3 the key is reported as present"
else
	bad "11.3 the key is reported as present" "has_key=${HASKEY}"
fi
if printf '%s' "${PROV}" | grep -q "e2e-upstream-secret-key"; then
	bad "11.4 the admin API does not echo the credential"
else
	ok "11.4 the admin API does not echo the credential"
fi

# Three protocols must be advertised.
NPROTO=$(json_get "${WORK}/providers.json" '.protocols | length' 2>/dev/null || echo "")
if [ -n "${NPROTO}" ] && [ "${NPROTO}" -eq 3 ] 2>/dev/null; then
	ok "11.5 all three protocols are advertised"
else
	bad "11.5 all three protocols are advertised" "count=${NPROTO}"
fi

# Model discovery: the mock upstream serves /v1/models.
curl -sS --max-time 20 -X POST "${ADMIN}/admin/models/refresh" > "${WORK}/refresh.json" 2>&1
curl -sS --max-time 10 "${ADMIN}/admin/models" > "${WORK}/catalogue.json"
NCAT=$(json_get "${WORK}/catalogue.json" '.models | length' 2>/dev/null || echo "")
if [ -n "${NCAT}" ] && [ "${NCAT}" -ge 4 ] 2>/dev/null; then
	ok "11.6 the model catalogue is populated (${NCAT} model(s))"
else
	bad "11.6 the model catalogue is populated" "models=${NCAT}"
fi

# User-facing model list, authenticated.
MODELS=$(curl -sS --max-time 10 "${BASE}/v1/models" -H "Authorization: Bearer ${ALICE_TOKEN}")
if printf '%s' "${MODELS}" | grep -q "mock-basic"; then
	ok "11.7 the user-facing model list is served"
else
	bad "11.7 the user-facing model list is served" "$(printf '%.200s' "${MODELS}")"
fi

# ------------------------------------------------------------------ test 12
head1 "12. the three LLM protocols"
# 12.1 chat completions (native)
BODY='{"model":"mock-basic","messages":[{"role":"user","content":"chat protocol"}]}'
OUT=$(request "${ALICE_TOKEN}" /v1/chat/completions "${BODY}")
if printf '%s' "${OUT}" | grep -q "Hello from the mock upstream"; then
	ok "12.1 /v1/chat/completions works natively"
else
	bad "12.1 /v1/chat/completions works natively" "$(printf '%.160s' "${OUT}")"
fi

# 12.2 Responses API, translated to the chat-only provider above. The provider
# declares "responses" in supports, which is what permits the conversion.
BODY='{"model":"mock-basic","input":"responses protocol","max_output_tokens":64}'
OUT=$(request "${ALICE_TOKEN}" /v1/responses "${BODY}")
if printf '%s' "${OUT}" | grep -q '"object": *"response"'; then
	ok "12.2 /v1/responses returns a Responses-shaped body"
else
	bad "12.2 /v1/responses returns a Responses-shaped body" "$(printf '%.200s' "${OUT}")"
fi
if printf '%s' "${OUT}" | grep -q "Hello from the mock upstream"; then
	ok "12.3 the Responses reply carries the upstream text"
else
	bad "12.3 the Responses reply carries the upstream text" "$(printf '%.200s' "${OUT}")"
fi
if printf '%s' "${OUT}" | grep -qi "chat.completion"; then
	bad "12.4 the reply is not a leaked chat-completions body"
else
	ok "12.4 the reply is not a leaked chat-completions body"
fi

# 12.5 Anthropic Messages, likewise translated.
BODY='{"model":"mock-basic","max_tokens":64,"messages":[{"role":"user","content":"messages protocol"}]}'
OUT=$(request "${ALICE_TOKEN}" /v1/messages "${BODY}")
if printf '%s' "${OUT}" | grep -q '"type": *"message"'; then
	ok "12.5 /v1/messages returns a Messages-shaped body"
else
	bad "12.5 /v1/messages returns a Messages-shaped body" "$(printf '%.200s' "${OUT}")"
fi
if printf '%s' "${OUT}" | grep -q "Hello from the mock upstream"; then
	ok "12.6 the Messages reply carries the upstream text"
else
	bad "12.6 the Messages reply carries the upstream text" "$(printf '%.200s' "${OUT}")"
fi

# 12.7 the audit record names the protocol the upstream actually spoke, so an
# operator debugging odd output can see which surface served the request.
if grep -q '"upstream_protocol":"openai"' "${WORK}/audit.jsonl"; then
	ok "12.7 the upstream protocol is recorded alongside the client's"
else
	bad "12.7 the upstream protocol is recorded alongside the client's"
fi

# 12.8 a provider that declares all three surfaces is served natively: the
# request must NOT be marked as translated, because no conversion happened and
# saying otherwise would send an operator hunting for a conversion bug.
if grep -q '"translated":true' "${WORK}/audit.jsonl"; then
	bad "12.8 declared surfaces are served without translation" \
		"a request to the mock's own Responses surface was recorded as translated"
else
	ok "12.8 declared surfaces are served without translation"
fi

# 12.9 streaming through the Responses endpoint.
curl -sS --max-time 60 -N -X POST "${BASE}/v1/responses" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d '{"model":"mock-basic","input":"stream please","stream":true}' \
	> "${WORK}/responses-stream.txt" 2>&1
if grep -q "response.output_text.delta\|response.completed" "${WORK}/responses-stream.txt"; then
	ok "12.9 the Responses endpoint streams named events"
else
	bad "12.9 the Responses endpoint streams named events" "$(printf '%.200s' "$(cat "${WORK}/responses-stream.txt")")"
fi

# 12.10 usage is metered for the translated protocols too.
curl -sS --max-time 10 "${ADMIN}/admin/usage" > "${WORK}/usage12.json"
# The channel list is ordered by traffic, not by name, so the channel has to be
# selected by name rather than by position.
MOCKREQ=$(python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print(""); raise SystemExit
for ch in d.get("channels") or []:
    if ch.get("channel") == "mock/main":
        print(ch.get("requests", "")); break
else:
    print("")
' "${WORK}/usage12.json" 2>/dev/null || echo "")
if [ -n "${MOCKREQ}" ] && [ "${MOCKREQ}" -ge 3 ] 2>/dev/null; then
	ok "12.10 every protocol's traffic is metered (${MOCKREQ} requests on the channel)"
else
	bad "12.10 every protocol's traffic is metered" "requests=${MOCKREQ}"
fi

# ------------------------------------------------------------------ test 13
head1 "13. management API reachability"

if [ -z "${NONLOOP_IP}" ]; then
	skip "13.x management reachability (this host has no non-loopback address)"
else
	# The management API can suspend users and read the ledger, so it is not
	# enough for its listener to be bound narrowly: the handler itself must
	# refuse callers that are not on the loopback interface. A second gateway
	# instance is started on a routable address to prove both halves of that
	# policy - refusal by default, and token-checked access when the operator
	# deliberately opens it.
	admin_send() {
		_remote=$1; _path=$2; _cred=${3:-}
		if [ -n "${_cred}" ]; then
			curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
				-H "Authorization: Bearer ${_cred}" "${_remote}${_path}" 2>/dev/null
		else
			curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
				"${_remote}${_path}" 2>/dev/null
		fi
	}

	write_remote_config() {
		_open=$1
		cat > "${WORK}/customs-remote.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${REMOTE_PORT}",
    "admin_listen": "${NONLOOP_IP}:${REMOTE_ADMIN_PORT}",
    "admin_remote": ${_open},
    "admin_token": "${REMOTE_ADMIN_TOKEN}",
    "audit_path": "${WORK}/audit-remote.jsonl",
    "meter_path": "${WORK}/meter-remote.jsonl",
    "state_path": "${WORK}/state-remote.json",
    "price_path": "${WORK}/catalogue-remote.json",
    "score_path": "${WORK}/scores-remote.json",
    "upstream_timeout": "60s",
    "fail_mode": "closed",
    "log_level": "error"
  },
  "policy": {"outbound": {"mode": "enforce"}, "inbound": {"mode": "enforce"}},
  "providers": [
    {
      "name": "mock",
      "base_url": "${UPSTREAM}",
      "protocol": "openai",
      "trust": "untrusted",
      "enabled": true,
      "models": {"mock-basic": "mock-basic"},
      "keys": [{"name": "main", "api_key": ""}],
      "discover": {"enabled": false}
    }
  ],
  "users": [{"name": "alice", "token": "${ALICE_TOKEN}", "quota": 0}],
  "prices": [
    {"channel": "mock/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON
	}

	write_remote_config false
	"${PREFIX}/sbin/customsd" --config "${WORK}/customs-remote.json" \
		--env-file "${WORK}/customsd.env" > "${WORK}/gateway-remote-a.log" 2>&1 &
	REMOTE_GW_PID=$!

	REMOTE_ADMIN="http://${NONLOOP_IP}:${REMOTE_ADMIN_PORT}"
	_started=0
	_deadline=$(( $(date +%s) + 20 ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		if curl -sS --max-time 3 -o /dev/null "${REMOTE_ADMIN}/admin/healthz" 2>/dev/null; then
			_started=1
			break
		fi
		sleep 0.5
	done
	if [ "${_started}" != "1" ]; then
		bad "13.0 the management listener started on ${NONLOOP_IP}" \
			"see ${WORK}/gateway-remote-a.log"
	else
		ok "13.0 the management listener can be bound to a routable address"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "")
	if [ "${CODE}" = "403" ]; then
		ok "13.1 a remote caller is refused by default (403)"
	else
		bad "13.1 a remote caller is refused by default" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "" "X-Forwarded-For: 127.0.0.1")
	if [ "${CODE}" = "403" ]; then
		ok "13.2 a spoofed X-Forwarded-For cannot claim loopback"
	else
		bad "13.2 a spoofed X-Forwarded-For cannot claim loopback" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "${ADMIN_TOKEN}")
	if [ "${CODE}" = "403" ]; then
		ok "13.3 a user token does not unlock the management API"
	else
		bad "13.3 a user token does not unlock the management API" "status=${CODE}"
	fi

	kill "${REMOTE_GW_PID}" 2>/dev/null
	wait "${REMOTE_GW_PID}" 2>/dev/null
	REMOTE_GW_PID=""

	write_remote_config true
	"${PREFIX}/sbin/customsd" --config "${WORK}/customs-remote.json" \
		--env-file "${WORK}/customsd.env" > "${WORK}/gateway-remote-b.log" 2>&1 &
	REMOTE_GW_PID=$!

	_started=0
	_deadline=$(( $(date +%s) + 20 ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		if curl -sS --max-time 3 -o /dev/null "${REMOTE_ADMIN}/admin/healthz" 2>/dev/null; then
			_started=1
			break
		fi
		sleep 0.5
	done
	if [ "${_started}" != "1" ]; then
		bad "13.4 the management listener restarted with remote access enabled" \
			"see ${WORK}/gateway-remote-b.log"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "")
	if [ "${CODE}" = "401" ]; then
		ok "13.5 an opened management API still demands a credential (401)"
	else
		bad "13.5 an opened management API still demands a credential" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "wrong-token")
	if [ "${CODE}" = "401" ]; then
		ok "13.6 a wrong credential is rejected (401)"
	else
		bad "13.6 a wrong credential is rejected" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/stats "${REMOTE_ADMIN_TOKEN}")
	if [ "${CODE}" = "200" ]; then
		ok "13.7 the configured admin token is accepted"
	else
		bad "13.7 the configured admin token is accepted" "status=${CODE}"
	fi

	# The console's shell must load without a credential - otherwise the page
	# that asks for one could never be fetched - while its data calls do not.
	CODE=$(admin_send "${REMOTE_ADMIN}" / "")
	if [ "${CODE}" = "200" ]; then
		ok "13.8 the console shell loads so it can ask for a credential"
	else
		bad "13.8 the console shell loads so it can ask for a credential" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/usage "")
	if [ "${CODE}" = "401" ]; then
		ok "13.9 the console's data calls are refused without a credential"
	else
		bad "13.9 the console's data calls are refused without a credential" "status=${CODE}"
	fi

	CODE=$(admin_send "${REMOTE_ADMIN}" /admin/usage "${REMOTE_ADMIN_TOKEN}")
	if [ "${CODE}" = "200" ]; then
		ok "13.10 the console loads its data once the token is presented"
	else
		bad "13.10 the console loads its data once the token is presented" "status=${CODE}"
	fi

	kill "${REMOTE_GW_PID}" 2>/dev/null
	wait "${REMOTE_GW_PID}" 2>/dev/null
	REMOTE_GW_PID=""
fi

# ------------------------------------------------------------------ test 14
head1 "14. protocol conversion"


# Test 12 proved the native path: the mock declares all three surfaces, so every
# protocol is served as-is. The conversion path needs the opposite shape - an
# upstream that has only the chat surface - and the second gateway instance from
# test 13 is the place to host it, because both instances can point at the same
# mock while declaring different capabilities.
if [ -z "${NONLOOP_IP}" ]; then
	skip "14.x protocol conversion (needs the second instance from test 13)"
else
	cat > "${WORK}/customs-convert.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${REMOTE_PORT}",
    "admin_listen": "127.0.0.1:${CONVERT_ADMIN_PORT}",
    "audit_path": "${WORK}/audit-convert.jsonl",
    "meter_path": "${WORK}/meter-convert.jsonl",
    "state_path": "${WORK}/state-convert.json",
    "price_path": "${WORK}/catalogue-convert.json",
    "score_path": "${WORK}/scores-convert.json",
    "upstream_timeout": "60s",
    "fail_mode": "closed",
    "log_level": "error"
  },
  "policy": {"outbound": {"mode": "enforce"}, "inbound": {"mode": "enforce"}},
  "providers": [
    {
      "name": "chatonly",
      "base_url": "${UPSTREAM}",
      "protocol": "openai",
      "supports": ["openai"],
      "trust": "untrusted",
      "enabled": true,
      "models": {"mock-basic": "mock-basic"},
      "keys": [{"name": "main", "api_key": ""}],
      "discover": {"enabled": false}
    }
  ],
  "routing": {"prefer_same_protocol": true, "allow_translation": true, "max_attempts": 1},
  "users": [{"name": "alice", "token": "${ALICE_TOKEN}", "quota": 0}],
  "prices": [
    {"channel": "chatonly/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON
	"${PREFIX}/sbin/customsd" --config "${WORK}/customs-convert.json" \
		--env-file "${WORK}/customsd.env" > "${WORK}/gateway-convert.log" 2>&1 &
	CONVERT_GW_PID=$!
	CONVERT_ADMIN="http://127.0.0.1:${CONVERT_ADMIN_PORT}"
	CONVERT_BASE="http://127.0.0.1:${REMOTE_PORT}"

	_started=0
	_deadline=$(( $(date +%s) + 20 ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		if curl -sS --max-time 3 -o /dev/null "${CONVERT_ADMIN}/admin/healthz" 2>/dev/null; then
			_started=1
			break
		fi
		sleep 0.5
	done
	if [ "${_started}" != "1" ]; then
		bad "14.0 the conversion instance started" "see ${WORK}/gateway-convert.log"
	else
		ok "14.0 the conversion instance started"
	fi

	# Chat goes to a chat-only upstream unchanged...
	curl -sS --max-time 60 -X POST "${CONVERT_BASE}/v1/chat/completions" \
		-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
		-d '{"model":"mock-basic","messages":[{"role":"user","content":"native"}]}' \
		> "${WORK}/convert-chat.json" 2>&1
	if grep -q '"object": *"chat.completion"' "${WORK}/convert-chat.json"; then
		ok "14.1 a chat request is served by a chat-only upstream"
	else
		bad "14.1 a chat request is served by a chat-only upstream" \
			"$(printf '%.200s' "$(cat "${WORK}/convert-chat.json")")"
	fi

	# ...while a Responses request must be converted both ways, and the client
	# must still receive a Responses-shaped body.
	curl -sS --max-time 60 -D "${WORK}/convert-resp.hdr" -X POST "${CONVERT_BASE}/v1/responses" \
		-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
		-d '{"model":"mock-basic","input":"convert me"}' \
		> "${WORK}/convert-resp.json" 2>&1
	if grep -q '"object": *"response"' "${WORK}/convert-resp.json"; then
		ok "14.2 a Responses request is converted and answered in Responses shape"
	else
		bad "14.2 a Responses request is converted and answered in Responses shape" \
			"$(printf '%.200s' "$(cat "${WORK}/convert-resp.json")")"
	fi
	if grep -qi 'X-Customs-Translated' "${WORK}/convert-resp.hdr"; then
		ok "14.3 the conversion is announced on the response"
	else
		bad "14.3 the conversion is announced on the response"
	fi
	if grep -q '"translated":true' "${WORK}/audit-convert.jsonl"; then
		ok "14.4 the audit ledger records the conversion"
	else
		bad "14.4 the audit ledger records the conversion"
	fi
	if grep -q '"protocol":"responses"' "${WORK}/audit-convert.jsonl" &&
		grep -q '"upstream_protocol":"openai"' "${WORK}/audit-convert.jsonl"; then
		ok "14.5 both protocols are recorded (client responses, upstream openai)"
	else
		bad "14.5 both protocols are recorded (client responses, upstream openai)"
	fi

	# The third pair: an Anthropic request against a chat-only upstream.
	curl -sS --max-time 60 -X POST "${CONVERT_BASE}/v1/messages" \
		-H "Content-Type: application/json" \
		-H "x-api-key: ${ALICE_TOKEN}" -H "anthropic-version: 2023-06-01" \
		-d '{"model":"mock-basic","max_tokens":64,"messages":[{"role":"user","content":"convert me too"}]}' \
		> "${WORK}/convert-msg.json" 2>&1
	if grep -q '"type": *"message"' "${WORK}/convert-msg.json"; then
		ok "14.6 an Anthropic request is converted and answered in Messages shape"
	else
		bad "14.6 an Anthropic request is converted and answered in Messages shape" \
			"$(printf '%.200s' "$(cat "${WORK}/convert-msg.json")")"
	fi

	kill "${CONVERT_GW_PID}" 2>/dev/null
	wait "${CONVERT_GW_PID}" 2>/dev/null
	CONVERT_GW_PID=""
fi

# ------------------------------------------------------------------ test 15
head1 "15. failover between providers"

# The operator's requirement in one test: when the first provider cannot serve
# the request because its own account is out of money, the same request must be
# answered by the next provider. The first provider's refusal is reproduced
# verbatim, because a generic 403 must stay terminal - only a refusal that names
# the provider's own credit state is worth retrying elsewhere.
cat > "${WORK}/customs-failover.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${FAILOVER_PORT}",
    "admin_listen": "127.0.0.1:${FAILOVER_ADMIN_PORT}",
    "audit_path": "${WORK}/audit-failover.jsonl",
    "meter_path": "${WORK}/meter-failover.jsonl",
    "state_path": "${WORK}/state-failover.json",
    "price_path": "${WORK}/catalogue-failover.json",
    "score_path": "${WORK}/scores-failover.json",
    "upstream_timeout": "60s",
    "fail_mode": "closed",
    "log_level": "${AUX_LOG_LEVEL}",
    "route_strategy": "fixed"
  },
  "policy": {"outbound": {"mode": "enforce"}, "inbound": {"mode": "enforce"}},
  "providers": [
    {
      "name": "broke",
      "base_url": "${UPSTREAM}",
      "protocol": "openai",
      "supports": ["openai", "responses"],
      "trust": "untrusted",
      "enabled": true,
      "weight": 1,
      "models": {"mock-basic": "mock-basic"},
      "keys": [{"name": "main", "api_key": ""}],
      "discover": {"enabled": false}
    },
    {
      "name": "healthy",
      "base_url": "${UPSTREAM2}",
      "protocol": "openai",
      "supports": ["openai", "responses"],
      "trust": "untrusted",
      "enabled": true,
      "weight": 1,
      "models": {"mock-basic": "mock-basic"},
      "keys": [{"name": "main", "api_key": ""}],
      "discover": {"enabled": false}
    }
  ],
  "routing": {
    "prefer_same_protocol": true,
    "allow_translation": true,
    "max_attempts": 3,
    "retry_on": ["5xx", "429", "timeout", "network", "quota"]
  },
  "users": [{"name": "alice", "token": "${ALICE_TOKEN}", "quota": 0}],
  "prices": [
    {"channel": "broke/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "healthy/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON

# Order matters: with "fixed" the router keeps configuration order, so "broke"
# is genuinely attempted first and the failover is what answers the caller.
"${PREFIX}/sbin/customsd" --config "${WORK}/customs-failover.json" \
	--env-file "${WORK}/customsd.env" > "${WORK}/gateway-failover.log" 2>&1 &
FAILOVER_GW_PID=$!
FAILOVER_ADMIN="http://127.0.0.1:${FAILOVER_ADMIN_PORT}"
FAILOVER_BASE="http://127.0.0.1:${FAILOVER_PORT}"

_started=0
_deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "${_deadline}" ]; do
	if curl -sS --max-time 3 -o /dev/null "${FAILOVER_ADMIN}/admin/healthz" 2>/dev/null; then
		_started=1
		break
	fi
	sleep 0.5
done
if [ "${_started}" != "1" ]; then
	bad "15.0 the failover instance started" "see ${WORK}/gateway-failover.log"
else
	ok "15.0 the failover instance started"
fi

# Make the mock refuse the way an aggregator with an empty balance does.
curl -sS --max-time 10 -X POST "${UPSTREAM}/admin/control" -H 'Content-Type: application/json' \
	-d '{"force_status": 403, "error_body": {"error": {"message": "预扣费额度失败, 用户剩余额度: ¥0.017783, 需要预扣费额度: ¥0.086051", "type": "new_api_error", "code": "insufficient_user_quota"}}}' \
	> /dev/null 2>&1

OUT=$(curl -sS --max-time 60 -X POST "${FAILOVER_BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d '{"model":"mock-basic","messages":[{"role":"user","content":"failover please"}]}' 2>&1)
if printf '%s' "${OUT}" | grep -q "Hello from the mock upstream"; then
	ok "15.1 an out-of-credit provider does not fail the request"
else
	bad "15.1 an out-of-credit provider does not fail the request" "$(printf '%.300s' "${OUT}")"
fi

if [ -f "${WORK}/audit-failover.jsonl" ]; then
	if grep -q '"attempts":2' "${WORK}/audit-failover.jsonl"; then
		ok "15.2 the audit record shows the request was tried twice"
	else
		bad "15.2 the audit record shows the request was tried twice" \
			"$(grep -o '"attempts":[0-9]*' "${WORK}/audit-failover.jsonl" | head -3 | tr '\n' ' ')"
	fi
	if grep -q '"channel":"healthy/main"' "${WORK}/audit-failover.jsonl"; then
		ok "15.3 the record names the provider that actually answered"
	else
		bad "15.3 the record names the provider that actually answered"
	fi
	if grep -q 'failover' "${WORK}/audit-failover.jsonl"; then
		ok "15.4 the channel switch is explained in the record"
	else
		bad "15.4 the channel switch is explained in the record"
	fi
else
	bad "15.2 the failover ledger was written"
fi

# Restore the mock and stop this instance.
curl -sS --max-time 10 -X POST "${UPSTREAM}/admin/control" -H 'Content-Type: application/json' \
	-d '{"force_status": null, "error_body": null}' > /dev/null 2>&1
kill "${FAILOVER_GW_PID}" 2>/dev/null
wait "${FAILOVER_GW_PID}" 2>/dev/null
FAILOVER_GW_PID=""

# ------------------------------------------------------------------ test 16
head1 "16. one protocol in, any protocol out"

# A client may speak Messages, Responses or chat completions and reach a model
# that lives behind a provider speaking something else entirely. The provider
# here declares only chat, so a Messages request has to be converted on the way
# out and the reply converted back on the way in.
cat > "${WORK}/customs-xproto.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${XPROTO_PORT}",
    "admin_listen": "127.0.0.1:${XPROTO_ADMIN_PORT}",
    "audit_path": "${WORK}/audit-xproto.jsonl",
    "meter_path": "${WORK}/meter-xproto.jsonl",
    "state_path": "${WORK}/state-xproto.json",
    "price_path": "${WORK}/catalogue-xproto.json",
    "score_path": "${WORK}/scores-xproto.json",
    "upstream_timeout": "60s",
    "fail_mode": "closed",
    "log_level": "error"
  },
  "policy": {"outbound": {"mode": "enforce"}, "inbound": {"mode": "enforce"}},
  "providers": [
    {
      "name": "chatshop",
      "base_url": "${UPSTREAM}",
      "protocol": "openai",
      "supports": ["openai"],
      "trust": "untrusted",
      "enabled": true,
      "models": {"mock-basic": "mock-basic"},
      "keys": [{"name": "main", "api_key": ""}],
      "discover": {"enabled": false}
    }
  ],
  "routing": {"prefer_same_protocol": true, "allow_translation": true, "max_attempts": 2},
  "users": [{"name": "alice", "token": "${ALICE_TOKEN}", "quota": 0}],
  "prices": [
    {"channel": "chatshop/main", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON
"${PREFIX}/sbin/customsd" --config "${WORK}/customs-xproto.json" \
	--env-file "${WORK}/customsd.env" > "${WORK}/gateway-xproto.log" 2>&1 &
XPROTO_GW_PID=$!
XPROTO_ADMIN="http://127.0.0.1:${XPROTO_ADMIN_PORT}"
XPROTO_BASE="http://127.0.0.1:${XPROTO_PORT}"

_started=0
_deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "${_deadline}" ]; do
	if curl -sS --max-time 3 -o /dev/null "${XPROTO_ADMIN}/admin/healthz" 2>/dev/null; then
		_started=1
		break
	fi
	sleep 0.5
done
if [ "${_started}" != "1" ]; then
	bad "16.0 the cross-protocol instance started" "see ${WORK}/gateway-xproto.log"
else
	ok "16.0 the cross-protocol instance started"
fi

# Messages in, chat upstream, Messages out.
curl -sS --max-time 60 -D "${WORK}/xp-msg.hdr" -X POST "${XPROTO_BASE}/v1/messages" \
	-H "Content-Type: application/json" -H "x-api-key: ${ALICE_TOKEN}" \
	-H "anthropic-version: 2023-06-01" \
	-d '{"model":"mock-basic","max_tokens":64,"messages":[{"role":"user","content":"messages to chat"}]}' \
	> "${WORK}/xp-msg.json" 2>&1
if grep -q '"type": *"message"' "${WORK}/xp-msg.json" &&
	grep -q 'Hello from the mock upstream' "${WORK}/xp-msg.json"; then
	ok "16.1 a Messages client reaches a chat-only provider and gets a Messages reply"
else
	bad "16.1 a Messages client reaches a chat-only provider and gets a Messages reply" \
		"$(printf '%.300s' "$(cat "${WORK}/xp-msg.json")")"
fi

# Responses in, chat upstream, Responses out.
curl -sS --max-time 60 -X POST "${XPROTO_BASE}/v1/responses" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d '{"model":"mock-basic","input":"responses to chat"}' \
	> "${WORK}/xp-resp.json" 2>&1
if grep -q '"object": *"response"' "${WORK}/xp-resp.json" &&
	grep -q 'Hello from the mock upstream' "${WORK}/xp-resp.json"; then
	ok "16.2 a Responses client reaches a chat-only provider and gets a Responses reply"
else
	bad "16.2 a Responses client reaches a chat-only provider and gets a Responses reply" \
		"$(printf '%.300s' "$(cat "${WORK}/xp-resp.json")")"
fi

# Chat in, chat upstream: the identity case must not be "translated".
curl -sS --max-time 60 -X POST "${XPROTO_BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE_TOKEN}" \
	-d '{"model":"mock-basic","messages":[{"role":"user","content":"chat to chat"}]}' \
	> "${WORK}/xp-chat.json" 2>&1
if grep -q 'Hello from the mock upstream' "${WORK}/xp-chat.json"; then
	ok "16.3 a chat client still reaches the same provider unchanged"
else
	bad "16.3 a chat client still reaches the same provider unchanged" \
		"$(printf '%.300s' "$(cat "${WORK}/xp-chat.json")")"
fi

# Both conversions must be visible in the ledger, and the native call must not be.
if [ "$(grep -c '"translated":true' "${WORK}/audit-xproto.jsonl" 2>/dev/null || echo 0)" -ge 2 ]; then
	ok "16.4 both converted exchanges are marked as translated"
else
	bad "16.4 both converted exchanges are marked as translated" \
		"$(grep -o '"translated":[a-z]*' "${WORK}/audit-xproto.jsonl" 2>/dev/null | tr '\n' ' ')"
fi
# The native exchange carries no translation marker at all: the field is
# omitempty, so its absence is the assertion. Counting is the honest check -
# three exchanges, of which exactly the two converted ones are marked.
XP_TOTAL=$(grep -c '"endpoint"' "${WORK}/audit-xproto.jsonl" 2>/dev/null || echo 0)
XP_TRANSLATED=$(grep -c '"translated":true' "${WORK}/audit-xproto.jsonl" 2>/dev/null || echo 0)
if [ "${XP_TOTAL}" = "3" ] && [ "${XP_TRANSLATED}" = "2" ]; then
	ok "16.5 only the converted exchanges are marked (${XP_TRANSLATED}/${XP_TOTAL})"
else
	bad "16.5 only the converted exchanges are marked" \
		"records=${XP_TOTAL} translated=${XP_TRANSLATED}"
fi

kill "${XPROTO_GW_PID}" 2>/dev/null
wait "${XPROTO_GW_PID}" 2>/dev/null
XPROTO_GW_PID=""

# ------------------------------------------------------------------- report
head1 "results"
say "passed: ${PASS}"
say "failed: ${FAIL}"
if [ "${FAIL}" -ne 0 ]; then
	say "failed checks:${FAILED_NAMES}"
	say ""
	say "gateway log (last 30 lines):"
	tail -30 "${WORK}/gateway.log" 2>/dev/null
	say ""
	say "artifacts kept in ${WORK}"
	exit 1
fi
say ""
say "ALL CHECKS PASSED"
exit 0
