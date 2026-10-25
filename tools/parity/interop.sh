#!/bin/sh
# Ledger interoperability: each implementation verifies the other's ledger.
[ -n "$GO_REF" ] || GO_REF="$HOME/.cache/go-ref"
[ -n "$RS_REF" ] || RS_REF="$HOME/.cache/customs-prefix"
set -u
WORK=$(mktemp -d)
MOCK_PORT=19101
GO_PORT=19099; GO_ADMIN=19100
RS_PORT=19102; RS_ADMIN=19103
TOKEN=e2e-interop-token
UPKEY=interop-upstream-key
GO=$GO_REF
RS=$RS_REF
PASS=0; FAIL=0

mkcfg() {
  cat > "$1" <<JSON
{
  "_comment": "interop fixture",
  "server": {
    "listen": "127.0.0.1:$2",
    "admin_listen": "127.0.0.1:$3",
    "audit_path": "$4",
    "meter_path": "$4.meter",
    "state_path": "$4.state",
    "price_path": "$4.prices",
    "score_path": "$4.scores",
    "upstream_timeout": "30s",
    "fail_mode": "open"
  },
  "policy": {"outbound": {"mode": "enforce"}, "inbound": {"mode": "enforce"}},
  "channels": [{"name": "mock", "base_url": "http://127.0.0.1:$MOCK_PORT", "api_key": "$UPKEY"}],
  "users": [{"name": "alice", "token": "$TOKEN", "quota": 100000000}],
  "prices": [{"channel": "mock", "model": "mock-basic", "price": {"input_per_m": 1, "output_per_m": 2, "currency": "CNY"}}]
}
JSON
}

check() { # name, command
  if eval "$2" >"$WORK/out.txt" 2>&1; then
    PASS=$((PASS+1)); printf '  [PASS] %s\n' "$1"
  else
    FAIL=$((FAIL+1)); printf '  [FAIL] %s\n' "$1"; sed 's/^/         /' "$WORK/out.txt" | head -6
  fi
}

mkdir -p "$WORK"
python3 "$RS/share/customapi/mock-upstream.py" "$MOCK_PORT" > "$WORK/mock.log" 2>&1 &
MOCK_PID=$!
sleep 1

# --- Go writes, Rust verifies -------------------------------------------
mkdir -p "$WORK/go" "$WORK/rs"; mkcfg "$WORK/go.json" "$GO_PORT" "$GO_ADMIN" "$WORK/go/audit.jsonl"
"$GO/customsd" -config "$WORK/go.json" > "$WORK/go-gw.log" 2>&1 &
GO_PID=$!
sleep 1.5
curl -sS --max-time 10 -X POST "http://127.0.0.1:$GO_PORT/v1/chat/completions" \
  -H "Content-Type: application/json" -H "Authorization: Bearer $TOKEN" \
  -d '{"model":"mock-basic","messages":[{"role":"user","content":"hello"}]}' > "$WORK/go-resp.json" 2>&1
kill "$GO_PID" 2>/dev/null; wait "$GO_PID" 2>/dev/null
sleep 0.5
check "Go ledger -> Rust customsctl audit verify" "'$RS/bin/customsctl' audit verify --ledger '$WORK/go/audit.jsonl'"
check "Go ledger has a record" "[ \$(wc -l < '$WORK/go/audit.jsonl') -ge 1 ]"

# --- Rust writes, Go verifies -------------------------------------------
mkcfg "$WORK/rs.json" "$RS_PORT" "$RS_ADMIN" "$WORK/rs/audit.jsonl"
"$RS/sbin/customsd" --config "$WORK/rs.json" > "$WORK/rs-gw.log" 2>&1 &
RS_PID=$!
sleep 1.5
curl -sS --max-time 10 -X POST "http://127.0.0.1:$RS_PORT/v1/chat/completions" \
  -H "Content-Type: application/json" -H "Authorization: Bearer $TOKEN" \
  -d '{"model":"mock-basic","messages":[{"role":"user","content":"hello"}]}' > "$WORK/rs-resp.json" 2>&1
kill "$RS_PID" 2>/dev/null; wait "$RS_PID" 2>/dev/null
sleep 0.5
check "Rust ledger -> Go customsctl audit verify" "'$GO/customsctl' audit verify -ledger '$WORK/rs/audit.jsonl'"
check "Rust ledger has a record" "[ \$(wc -l < '$WORK/rs/audit.jsonl') -ge 1 ]"

# --- Compare the two records field by field -----------------------------
python3 - "$WORK" <<'PY'
import json, sys, os
work = sys.argv[1]
def load(p):
    with open(p) as fh:
        return json.loads(fh.readline())
go = load(os.path.join(work, "go", "audit.jsonl"))
rs = load(os.path.join(work, "rs", "audit.jsonl"))
keys = sorted(set(go) | set(rs))
diffs = []
for k in keys:
    if k in ("seq", "prev_hash", "entry_hash", "time", "end_time", "id", "latency_ms", "route_reason", "request_hash", "response_hash", "quota_after"):
        continue
    if go.get(k) != rs.get(k):
        diffs.append((k, go.get(k), rs.get(k)))
print("  compared %d shared fields" % len([k for k in keys if k not in ("seq","prev_hash","entry_hash","time","end_time","id","latency_ms","route_reason","request_hash","response_hash","quota_after")]))
if diffs:
    print("  [FAIL] record fields differ:")
    for k, a, b in diffs:
        print("         %s: go=%r rust=%r" % (k, a, b))
    raise SystemExit(1)
print("  [PASS] record fields agree")
PY
if [ $? -ne 0 ]; then FAIL=$((FAIL+1)); else PASS=$((PASS+1)); fi

# --- Response bodies ----------------------------------------------------
python3 - "$WORK" <<'PY'
import json, sys, os
work = sys.argv[1]
go = json.load(open(os.path.join(work, "go-resp.json")))
rs = json.load(open(os.path.join(work, "rs-resp.json")))
def strip(d):
    d = dict(d)
    d.pop("id", None); d.pop("created", None)
    return d
if strip(go) == strip(rs):
    print("  [PASS] proxied response bodies agree")
else:
    print("  [FAIL] proxied response bodies differ")
    print("         go  =", json.dumps(strip(go), sort_keys=True)[:400])
    print("         rust=", json.dumps(strip(rs), sort_keys=True)[:400])
    raise SystemExit(1)
PY
if [ $? -ne 0 ]; then FAIL=$((FAIL+1)); else PASS=$((PASS+1)); fi

kill "$MOCK_PID" 2>/dev/null
printf '\ninterop: passed=%d failed=%d\nwork=%s\n' "$PASS" "$FAIL" "$WORK"
[ "$FAIL" -eq 0 ]

