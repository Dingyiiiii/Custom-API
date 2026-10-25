#!/bin/bash
# Differential check: same config, same mock upstream, one Go gateway and one Rust gateway.
[ -n "$GO_REF" ] || GO_REF="$HOME/.cache/go-ref"
[ -n "$RS_REF" ] || RS_REF="$HOME/.cache/customs-prefix"
set -u
WORK=$(mktemp -d)
MOCK=19201
GOP=19202; GOA=19203
RSP=19204; RSA=19205
TOKEN=diff-token; UPKEY=diff-upstream-key
GO=$GO_REF/customsd
RS=$RS_REF/sbin/customsd

mkcfg() { # file listen admin dir
  mkdir -p "$4"
  cat > "$1" <<JSON
{
  "server": {"listen": "127.0.0.1:$2", "admin_listen": "127.0.0.1:$3",
             "audit_path": "$4/audit.jsonl", "meter_path": "$4/meter.jsonl",
             "state_path": "$4/state.json", "price_path": "$4/prices.json",
             "score_path": "$4/scores.json", "upstream_timeout": "30s"},
  "policy": {"outbound": {"mode": "enforce", "block_keywords": ["forbidden-widget"]},
             "inbound": {"mode": "enforce"}},
  "channels": [{"name": "mock", "base_url": "http://127.0.0.1:$MOCK", "api_key": "$UPKEY"}],
  "users": [{"name": "alice", "token": "$TOKEN", "quota": 1000000, "period": "daily"}],
  "prices": [{"channel": "mock", "model": "mock-basic",
              "price": {"input_per_m": 1, "output_per_m": 2, "currency": "CNY",
                        "scheme": "flat", "source": "fixture"}}]
}
JSON
}

mkcfg "$WORK/go/customs.json" "$GOP" "$GOA" "$WORK/go"
mkcfg "$WORK/rs/customs.json" "$RSP" "$RSA" "$WORK/rs"
python3 $RS_REF/share/customapi/mock-upstream.py "$MOCK" > "$WORK/mock.log" 2>&1 &
MOCK_PID=$!
"$GO" -config "$WORK/go/customs.json" > "$WORK/go.log" 2>&1 &
GO_PID=$!
"$RS" --config "$WORK/rs/customs.json" > "$WORK/rs.log" 2>&1 &
RS_PID=$!
sleep 2

ask() { curl -sS --max-time 15 -X POST "http://127.0.0.1:$1/v1/chat/completions" \
  -H "Content-Type: application/json" -H "Authorization: Bearer $TOKEN" \
  -d '{"model":"mock-basic","messages":[{"role":"user","content":"hello there"}]}' > /dev/null; }
ask "$GOP"; ask "$RSP"

paths="/admin/stats /admin/config /admin/providers /admin/models /admin/prices \
/admin/prices/compare?model=mock-basic&input_tokens=1000&output_tokens=500 \
/admin/usage /admin/audit?limit=5 /admin/healthz"
i=0; DIFFS=0
for p in $paths; do
  i=$((i+1))
  curl -sS --max-time 15 "http://127.0.0.1:$GOA$p" > "$WORK/go$i.json" 2>/dev/null
  curl -sS --max-time 15 "http://127.0.0.1:$RSA$p" > "$WORK/rs$i.json" 2>/dev/null
  python3 - "$WORK/go$i.json" "$WORK/rs$i.json" "$p" <<'PY'
import json, sys, math
go_path, rs_path, endpoint = sys.argv[1:4]
# Values that cannot match between two processes by construction: identifiers,
# timestamps, ledger paths and hashes, listener ports (the fixtures use different
# ones), and timing measurements.
VOLATILE = {"uptime_s","audit_seq","started_at","time","end_time","updated_at","created_at",
            "id","request_hash","response_hash","quota_after","last_seen","period_start",
            "version","latency_ms","ttfb_ms","total_ms","upstream_ms","outbound_audit_ms",
            "inbound_audit_ms","fetch_ms","discovered_at","discovered","at","audit_head",
            "audit_path","meter_path","state_path","price_path","score_path","entry_hash",
            "prev_hash","listen","admin_listen","mean_latency_ms","max_latency_ms"}
# Transport failures are worded by net/http and reqwest respectively; keep the
# stable prefix (which is ours) and drop the library text.
PREFIXED = {"error","discover_error"}
def load(p):
    try:
        with open(p) as fh:
            return json.load(fh)
    except Exception as exc:
        return {"__unparsable__": str(exc)}
go, rs = load(go_path), load(rs_path)
def canon(v, key=""):
    if key in PREFIXED and isinstance(v, str):
        return v.split(":")[0]
    if key == "errors" and isinstance(v, dict):
        # Per-channel discovery failures: same wording caveat as above.
        return {k: x.split(":")[0] if isinstance(x, str) else x for k, x in v.items()}
    if isinstance(v, dict):
        return {k: canon(x, k) for k, x in v.items() if k not in VOLATILE}
    if isinstance(v, list):
        return [canon(x, key) for x in v]
    if isinstance(v, float) and v == int(v) and abs(v) < 1e15:
        return int(v)
    return v
a, b = canon(go), canon(rs)
if a == b:
    print("  [SAME] %s" % endpoint)
    raise SystemExit(0)
print("  [DIFF] %s" % endpoint)
def walk(x, y, prefix=""):
    if isinstance(x, dict) and isinstance(y, dict):
        for k in sorted(set(x) | set(y)):
            walk(x.get(k), y.get(k), prefix + "/" + k)
    elif isinstance(x, list) and isinstance(y, list) and len(x) == len(y):
        for idx, (xi, yi) in enumerate(zip(x, y)):
            walk(xi, yi, prefix + "/%d" % idx)
    elif x != y:
        print("         %-46s go=%r rust=%r" % (prefix or "/", x, y))
walk(a, b)
raise SystemExit(1)
PY
  if [ $? -ne 0 ]; then DIFFS=$((DIFFS+1)); fi
done

# user-facing catalogue
curl -sS --max-time 15 "http://127.0.0.1:$GOP/v1/models?detail=true" -H "Authorization: Bearer $TOKEN" > "$WORK/go-models.json"
curl -sS --max-time 15 "http://127.0.0.1:$RSP/v1/models?detail=true" -H "Authorization: Bearer $TOKEN" > "$WORK/rs-models.json"
if diff -q "$WORK/go-models.json" "$WORK/rs-models.json" >/dev/null; then echo "  [SAME] /v1/models"; else echo "  [DIFF] /v1/models"; diff "$WORK/go-models.json" "$WORK/rs-models.json" | head -4; fi

echo
echo "work=$WORK"
kill "$GO_PID" "$RS_PID" "$MOCK_PID" 2>/dev/null

