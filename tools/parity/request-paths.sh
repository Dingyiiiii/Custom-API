#!/bin/bash
# Behaviour differential: identical fixtures, one Go gateway, one Rust gateway.
[ -n "$GO_REF" ] || GO_REF="$HOME/.cache/go-ref"
[ -n "$RS_REF" ] || RS_REF="$HOME/.cache/customs-prefix"
set -u
WORK=$(mktemp -d)
MOCK=19301; GOP=19302; GOA=19303; RSP=19304; RSA=19305
TOKEN=diff-token; UPKEY=diff-upstream-key
GO=$GO_REF/customsd
RS=$RS_REF/sbin/customsd
PASS=0; FAIL=0

mkcfg() { mkdir -p "$4"; cat > "$1" <<JSON
{
  "server": {"listen": "127.0.0.1:$2", "admin_listen": "127.0.0.1:$3",
             "audit_path": "$4/audit.jsonl", "meter_path": "$4/meter.jsonl",
             "state_path": "$4/state.json", "price_path": "$4/prices.json",
             "score_path": "$4/scores.json", "upstream_timeout": "30s"},
  "policy": {"outbound": {"mode": "enforce", "block_keywords": ["forbidden-widget"]},
             "inbound": {"mode": "enforce", "tool_call_policy": "block"}},
  "channels": [{"name": "mock", "base_url": "http://127.0.0.1:$MOCK", "api_key": "$UPKEY"}],
  "users": [{"name": "alice", "token": "$TOKEN", "quota": 1000000}],
  "prices": [{"channel": "mock", "model": "mock-basic",
              "price": {"input_per_m": 1, "output_per_m": 2, "currency": "CNY"}}]
}
JSON
}
mkcfg "$WORK/go.json" "$GOP" "$GOA" "$WORK/go"
mkcfg "$WORK/rs.json" "$RSP" "$RSA" "$WORK/rs"
python3 $RS_REF/share/customapi/mock-upstream.py "$MOCK" > "$WORK/mock.log" 2>&1 &
MOCK_PID=$!
sleep 1
"$GO" -config "$WORK/go.json" > "$WORK/go.log" 2>&1 &
GO_PID=$!
"$RS" --config "$WORK/rs.json" > "$WORK/rs.log" 2>&1 &
RS_PID=$!
sleep 2

call() { # gateway-port method path auth body extra-header...
  local port=$1 method=$2 path=$3 auth=$4 body=$5; shift 5
  local args=(-sS --max-time 20 -o "$WORK/body" -D "$WORK/hdr" -w '%{http_code}' -X "$method")
  [ -n "$auth" ] && args+=(-H "Authorization: Bearer $auth")
  if [ -n "$body" ]; then args+=(-H 'Content-Type: application/json' -d "$body"); fi
  for h in "$@"; do args+=(-H "$h"); done
  curl "${args[@]}" "http://127.0.0.1:$port$path"
}

compare() { # name method path auth body
  name=$1; shift
  gs=$(call "$GOP" "$@"); gb=$(cat "$WORK/body"); gh=$(cat "$WORK/hdr"); cp "$WORK/body" "$WORK/gb"
  rs=$(call "$RSP" "$@"); rb=$(cat "$WORK/body"); rh=$(cat "$WORK/hdr")
  # Normalise: strip audit ids, request ids, timestamps, model ids.
  # Volatile by construction: request/response ids, timestamps, audit ids, and
  # the build identity string (rustc versus go).
  norm() {
    printf '%s' "$1" | sed -E 's/req_[0-9a-f]+/req_X/g; s/chatcmpl-[A-Za-z0-9]+/chatcmpl-X/g; s/"created":[0-9]+/"created":0/g; s/"customs_audit_id":"[^"]*"/"customs_audit_id":"X"/g; s/"version":"customapi [^"]*"/"version":"customapi X"/g'
  }
  hnorm() { printf '%s' "$1" | tr -d '\r' | grep -iE '^(content-type|x-customs-stream-mode|x-customs-translated):' | sort | tr 'A-Z' 'a-z'; }
  nb=$(norm "$gb"); nrb=$(norm "$rb"); nh=$(hnorm "$gh"); nrh=$(hnorm "$rh")
  if [ "$gs" = "$rs" ] && [ "$nb" = "$nrb" ] && [ "$nh" = "$nrh" ]; then
    PASS=$((PASS+1)); echo "  [SAME] $name (status=$gs)"
  else
    FAIL=$((FAIL+1)); echo "  [DIFF] $name (go=$gs rust=$rs)"
    [ "$nh" != "$nrh" ] && diff <(printf '%s\n' "$nh") <(printf '%s\n' "$nrh") | head -4 | sed 's/^/         hdr /'
    if [ "$nb" != "$nrb" ]; then
      printf '%s' "$nb" > "$WORK/nb"; printf '%s' "$nrb" > "$WORK/nrb"
      diff "$WORK/nb" "$WORK/nrb" | head -4 | cut -c1-200 | sed 's/^/         body /'
    fi
  fi
}

echo '=== error and success paths ==='
compare "no auth" POST /v1/chat/completions "" '{"model":"mock-basic","messages":[{"role":"user","content":"hi"}]}'
compare "bad token" POST /v1/chat/completions wrong-token '{"model":"mock-basic","messages":[{"role":"user","content":"hi"}]}'
compare "missing model" POST /v1/chat/completions "$TOKEN" '{"messages":[{"role":"user","content":"hi"}]}'
compare "blocked keyword" POST /v1/chat/completions "$TOKEN" '{"model":"mock-basic","messages":[{"role":"user","content":"build a forbidden-widget"}]}'
compare "basic chat" POST /v1/chat/completions "$TOKEN" '{"model":"mock-basic","messages":[{"role":"user","content":"hello"}]}'
compare "injection blocked" POST /v1/chat/completions "$TOKEN" '{"model":"mock-inject","messages":[{"role":"user","content":"summarise"}]}'
compare "tool call blocked" POST /v1/chat/completions "$TOKEN" '{"model":"mock-toolcall","messages":[{"role":"user","content":"do it"}]}'
compare "inflated usage" POST /v1/chat/completions "$TOKEN" '{"model":"mock-inflate","messages":[{"role":"user","content":"hello"}]}'
compare "unknown v1 endpoint" POST /v1/embeddings "$TOKEN" '{"model":"mock-basic","input":"x"}'
compare "models no auth" GET /v1/models "" ""
compare "service map" GET / " " ""
compare "readyz" GET /readyz "" ""

echo '=== streaming ==='
compare "stream basic" POST /v1/chat/completions "$TOKEN" '{"model":"mock-basic","stream":true,"messages":[{"role":"user","content":"hello"}]}'
compare "stream injection" POST /v1/chat/completions "$TOKEN" '{"model":"mock-inject","stream":true,"messages":[{"role":"user","content":"summarise"}]}'

echo
echo "behaviour: passed=$PASS failed=$FAIL work=$WORK"
kill "$GO_PID" "$RS_PID" "$MOCK_PID" 2>/dev/null

