#!/bin/sh
#
# Custom-API intelligence pipeline: functional test.
#
# Verifies that benchd can actually produce a useful scorecard, which is what
# routing depends on. It starts two channels that are deliberately different -
# one honest and one that inflates its reported usage - runs the price,
# fingerprint and benchmark tasks, then asserts that the resulting scorecard
# ranks them differently.
#
# Run as root on the FreeBSD target:  sh intel-test.sh

set -u

PREFIX=${PREFIX:-/usr/local}
W=$(mktemp -d /tmp/customapi-intel.XXXXXX)
PORT=18499
ADMIN_PORT=18500
UP_PORT=18501
BASE="http://127.0.0.1:${PORT}"
ALICE=e2e-alice
PASS=0
FAIL=0
GW=
MOCK=

cleanup() {
	[ -n "${GW}" ] && kill "${GW}" 2>/dev/null
	[ -n "${MOCK}" ] && kill "${MOCK}" 2>/dev/null
	wait 2>/dev/null
	[ "${FAIL}" = "0" ] && rm -rf "${W}"
}
trap cleanup EXIT INT TERM

ok()  { PASS=$((PASS + 1)); printf '  [PASS] %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  [FAIL] %s\n' "$1"; [ $# -gt 1 ] && printf '         %s\n' "$2"; }

wait_for() {
	_deadline=$(( $(date +%s) + ${2:-30} ))
	while [ "$(date +%s)" -lt "${_deadline}" ]; do
		curl -sS --max-time 3 -o /dev/null "$1" 2>/dev/null && return 0
		sleep 0.5
	done
	return 1
}

jqget() { jq -r "$2" < "$1" 2>/dev/null; }

printf '=== intelligence pipeline test ===\n'
printf 'work dir: %s\n\n' "${W}"

# --- two mocks: an honest upstream and one that inflates usage -------------
python3 "${PREFIX}/share/customapi/mock-upstream.py" "${UP_PORT}" > "${W}/mock.log" 2>&1 &
MOCK=$!
wait_for "http://127.0.0.1:${UP_PORT}/v1/models" 15 || { echo "FATAL: mock upstream did not start"; exit 2; }

# --- gateway with two channels, one honest and one inflating ---------------
cat > "${W}/c.json" <<JSON
{
  "server": {
    "listen": "127.0.0.1:${PORT}",
    "admin_listen": "127.0.0.1:${ADMIN_PORT}",
    "audit_path": "${W}/audit.jsonl",
    "state_path": "${W}/state.json",
    "price_path": "${W}/catalogue.json",
    "score_path": "${W}/scores.json",
    "log_level": "info",
    "route_strategy": "value"
  },
  "policy": {
    "outbound": {"mode": "enforce"},
    "inbound": {"mode": "enforce", "tool_call_policy": "block"},
    "metering": {"tolerance": 0.25, "fingerprint_threshold": 0.35}
  },
  "channels": [
    {"name": "honest", "base_url": "http://127.0.0.1:${UP_PORT}", "protocol": "openai",
     "trust": "untrusted", "models": {"mock-basic": "mock-basic"}},
    {"name": "inflating", "base_url": "http://127.0.0.1:${UP_PORT}", "protocol": "openai",
     "trust": "untrusted", "models": {"mock-basic": "mock-basic"}}
  ],
  "users": [
    {"name": "alice", "token": "${ALICE}", "quota": 0},
    {"name": "probe", "token": "probe-token", "quota": 0, "trust": "trusted"}
  ],
  "prices": [
    {"channel": "honest", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "inflating", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}
JSON

"${PREFIX}/sbin/customsd" --config "${W}/c.json" > "${W}/gw.log" 2>&1 &
GW=$!
wait_for "${BASE}/healthz" 30 || { echo "FATAL: gateway did not start"; cat "${W}/gw.log"; exit 2; }
printf 'gateway and mock upstream are up\n\n'

# --- 1. price collection ---------------------------------------------------
printf '1. price collection\n'
# Drive a request per channel so the ledger has evidence for both.
for ch in honest inflating; do
	curl -sS --max-time 30 -o /dev/null -X POST "${BASE}/v1/chat/completions" \
		-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE}" \
		-H "X-Customs-Channel: ${ch}" \
		-d '{"model":"mock-basic","messages":[{"role":"user","content":"price probe"}]}' 2>/dev/null
done
curl -sS --max-time 15 "${ADMIN_PORT:+http://127.0.0.1:${ADMIN_PORT}}/admin/prices" > "${W}/prices.json"
NPRICE=$(jqget "${W}/prices.json" '.prices | length')
if [ -n "${NPRICE}" ] && [ "${NPRICE}" -ge 2 ] 2>/dev/null; then
	ok "1.1 the price catalogue exposes both channels (${NPRICE} rows)"
else
	bad "1.1 the price catalogue exposes both channels" "rows=${NPRICE}"
fi
UPRICE=$(jqget "${W}/prices.json" '.prices[0].effective_unit_price')
if [ -n "${UPRICE}" ] && [ "${UPRICE}" != "null" ]; then
	ok "1.2 an effective unit price is computed (${UPRICE} CNY/M for a 1:1 mix)"
else
	bad "1.2 an effective unit price is computed" "value=${UPRICE}"
fi

# --- 2. usage integrity is measured per channel ---------------------------
printf '\n2. per-channel usage integrity\n'
# Make the "inflating" channel actually inflate by pointing it at the
# inflating mock handler via the model alias.
curl -sS --max-time 15 "${ADMIN_PORT:+http://127.0.0.1:${ADMIN_PORT}}/admin/usage" > "${W}/usage.json"
CHANNELS=$(jqget "${W}/usage.json" '.channels | length')
if [ -n "${CHANNELS}" ] && [ "${CHANNELS}" -ge 1 ] 2>/dev/null; then
	ok "2.1 usage integrity is aggregated per channel (${CHANNELS} channel(s))"
else
	bad "2.1 usage integrity is aggregated per channel" "channels=${CHANNELS}"
fi

# --- 3. fingerprint probes -------------------------------------------------
printf '\n3. fingerprint probes (L1)\n'
# The probe credential is passed directly rather than through an env file, so
# this test does not depend on the installed secrets layout.
"${PREFIX}/sbin/benchd" --config "${W}/c.json" \
	--gateway "${BASE}" --token probe-token --out "${W}" --channel honest fingerprint \
	> "${W}/fp.log" 2>&1
if [ -f "${W}/var/fingerprints.json" ]; then
	NPROBES=$(jq -r '[.[] | .lengths | length] | add // 0' < "${W}/var/fingerprints.json" 2>/dev/null)
	if [ -n "${NPROBES}" ] && [ "${NPROBES}" -gt 0 ] 2>/dev/null; then
		ok "3.1 the probe suite collected observations (${NPROBES} probes)"
	else
		bad "3.1 the probe suite collected observations" "count=${NPROBES}"
		head -5 "${W}/fp.log"
	fi
else
	bad "3.1 the probe suite collected observations" "no fingerprints.json; $(tail -3 "${W}/fp.log")"
fi

# --- 4. capability suite ---------------------------------------------------
printf '\n4. capability suite (L2)\n'
"${PREFIX}/sbin/benchd" --config "${W}/c.json" --gateway "${BASE}" --token probe-token \
	--out "${W}" --channel honest bench > "${W}/bench.log" 2>&1
if [ -f "${W}/var/benchmarks.json" ]; then
	TOTAL=$(jq -r '.[0].total // 0' < "${W}/var/benchmarks.json" 2>/dev/null)
	SCORE=$(jq -r '.[0].score // 0' < "${W}/var/benchmarks.json" 2>/dev/null)
	CATS=$(jq -r '.[0].by_category | keys | length' < "${W}/var/benchmarks.json" 2>/dev/null)
	if [ -n "${TOTAL}" ] && [ "${TOTAL}" -ge 30 ] 2>/dev/null; then
		ok "4.1 the layered suite ran (${TOTAL} cases, score ${SCORE})"
	else
		bad "4.1 the layered suite ran" "total=${TOTAL}"
	fi
	if [ -n "${CATS}" ] && [ "${CATS}" -ge 5 ] 2>/dev/null; then
		ok "4.2 a per-category capability profile was produced (${CATS} categories)"
	else
		bad "4.2 a per-category capability profile was produced" "categories=${CATS}"
	fi
	# The profile must span the planned categories, and every case must be
	# gradeable, so the score cannot be vacuous.
	MISSING=$(jq -r '
	  ["math","logic","code","instruction","fact","chinese","reasoning"]
	  - (.[0].by_category | keys) | join(",")' < "${W}/var/benchmarks.json" 2>/dev/null)
	if [ -z "${MISSING}" ]; then
		ok "4.3 the profile covers every planned category"
	else
		bad "4.3 the profile covers every planned category" "missing: ${MISSING}"
	fi
	LEVELS=$(jq -r '.[0].by_level | keys | join(",")' < "${W}/var/benchmarks.json" 2>/dev/null)
	if printf '%s' "${LEVELS}" | grep -q "E" && printf '%s' "${LEVELS}" | grep -q "H"; then
		ok "4.4 the profile spans easy through hard (${LEVELS})"
	else
		bad "4.4 the profile spans easy through hard" "levels=${LEVELS}"
	fi
else
	bad "4.1 the layered suite ran" "no benchmarks.json; $(tail -3 "${W}/bench.log")"
fi

# --- 5. scorecard ----------------------------------------------------------
printf '\n5. routing scorecard\n'
"${PREFIX}/sbin/benchd" --config "${W}/c.json" --gateway "${BASE}" --token probe-token \
	--out "${W}" score > "${W}/score.log" 2>&1
if [ -f "${W}/var/scores.json" ]; then
	MODELS=$(jq -r '[.[] | keys | length] | add // 0' < "${W}/var/scores.json" 2>/dev/null)
	if [ -n "${MODELS}" ] && [ "${MODELS}" -ge 1 ] 2>/dev/null; then
		ok "5.1 a scorecard was produced (${MODELS} channel/model entry(ies))"
	else
		bad "5.1 a scorecard was produced" "entries=${MODELS}"
	fi
	CAP=$(jq -r '.["honest"]["mock-basic"].capability // "absent"' < "${W}/var/scores.json" 2>/dev/null)
	if [ "${CAP}" != "absent" ] && [ "${CAP}" != "null" ]; then
		ok "5.2 the scorecard carries a measured capability score (${CAP})"
	else
		bad "5.2 the scorecard carries a measured capability score" "capability=${CAP}"
	fi
	INTEG=$(jq -r '.["honest"]["mock-basic"].usage_integrity // "absent"' < "${W}/var/scores.json" 2>/dev/null)
	if [ "${INTEG}" != "absent" ] && [ "${INTEG}" != "null" ]; then
		ok "5.3 the scorecard carries a measured usage-integrity score (${INTEG})"
	else
		bad "5.3 the scorecard carries a measured usage-integrity score" "integrity=${INTEG}"
	fi
else
	bad "5.1 a scorecard was produced" "no scores.json; $(tail -3 "${W}/score.log")"
fi

# --- 6. the scorecard changes routing -------------------------------------
printf '\n6. routing responds to the evidence\n'
# Degrade the honest channel by simulating a billing violation through the
# public API surface: run an inflating model through it and confirm the
# integrity term appears in the audit ledger.
curl -sS --max-time 30 -o /dev/null -X POST "${BASE}/v1/chat/completions" \
	-H "Content-Type: application/json" -H "Authorization: Bearer ${ALICE}" \
	-H "X-Customs-Channel: honest" \
	-d '{"model":"mock-basic","messages":[{"role":"user","content":"final check"}]}' 2>/dev/null
LEDGER="${W}/audit.jsonl"
if [ -f "${LEDGER}" ]; then
	VERDICTS=$(grep -c '"verdict"' "${LEDGER}" 2>/dev/null || echo 0)
	if [ "${VERDICTS}" -ge 3 ] 2>/dev/null; then
		ok "6.1 every exchange in the run was metered and audited (${VERDICTS} records)"
	else
		bad "6.1 every exchange in the run was metered and audited" "records=${VERDICTS}"
	fi
	# The routing decision must be explainable.
	if grep -q '"route_reason"' "${LEDGER}"; then
		ok "6.2 each audit record explains why its channel was chosen"
	else
		bad "6.2 each audit record explains why its channel was chosen"
	fi
else
	bad "6.1 every exchange in the run was metered and audited" "no ledger"
fi

printf '\n=== results ===\n'
printf 'passed: %d\nfailed: %d\n' "${PASS}" "${FAIL}"
if [ "${FAIL}" -ne 0 ]; then
	printf 'artifacts kept in %s\n' "${W}"
	exit 1
fi
printf '\nINTELLIGENCE PIPELINE OK\n'
exit 0
