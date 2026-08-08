#!/bin/bash
# End-to-end API test. Mints a FalconGo-shaped JWT and exercises the endpoints.
set -u
BASE="http://127.0.0.1:3099"
SECRET="testsecret"

TOKEN=$(python3 - "$SECRET" <<'PY'
import base64, hmac, hashlib, json, sys, time
secret = sys.argv[1].encode()
def b64(d): return base64.urlsafe_b64encode(d).rstrip(b'=')
# Exactly FalconGo's claim shape: no `sub`, numeric user_id, iss as a string.
header  = b64(json.dumps({"alg":"HS256","typ":"JWT"},separators=(',',':')).encode())
payload = b64(json.dumps({"user_type":"admin_user","user_id":7,"driver_id":0,
                          "permission":4,"iss":"7",
                          "exp":int(time.time())+3600},separators=(',',':')).encode())
signing = header + b'.' + payload
sig = b64(hmac.new(secret, signing, hashlib.sha256).digest())
print((signing + b'.' + sig).decode())
PY
)

AUTH="Authorization: Bearer $TOKEN"
JSON="Content-Type: application/json"

pass=0; fail=0
check() { # name expected actual
  if [ "$2" = "$3" ]; then echo "  PASS  $1"; pass=$((pass+1));
  else echo "  FAIL  $1 (expected $2, got $3)"; fail=$((fail+1)); fi
}

echo "=== health (no auth) ==="
check "healthz 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' $BASE/healthz)"
check "readyz 200"  200 "$(curl -s -o /dev/null -w '%{http_code}' $BASE/readyz)"

echo "=== auth ==="
check "no token -> 401" 401 "$(curl -s -o /dev/null -w '%{http_code}' $BASE/api/v1/transactions)"
check "tampered token -> 401" 401 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${TOKEN}x" $BASE/api/v1/transactions)"
check "valid token -> 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/transactions)"

echo "=== reads ==="
check "transactions 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" "$BASE/api/v1/transactions?limit=5")"
check "accounts 200"     200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/accounts)"
check "summary 200"      200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" "$BASE/api/v1/summary?group_by=day")"
check "summary bad group_by -> 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" "$BASE/api/v1/summary?group_by=; DROP TABLE")"
check "raw/review 200"   200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/raw/review)"
check "raw/ignored 200"  200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/raw/ignored)"
check "admin/templates 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/admin/templates)"
check "admin/ingest-status 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/admin/ingest-status)"

echo "=== manual create + validation ==="
CREATE='{"direction":"out","amount":"1234.56","currency":"EGP","occurred_at":"2026-08-01T10:00:00Z","counterparty":"Acme Parts","category":"Parts","description":"manual test row"}'
RESP=$(curl -s -H "$AUTH" -H "$JSON" -X POST -d "$CREATE" $BASE/api/v1/transactions)
TXID=$(echo "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null)
check "manual create returns id" "yes" "$([ -n "$TXID" ] && echo yes || echo no)"

check "negative amount -> 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST -d '{"direction":"out","amount":"-5","currency":"EGP","occurred_at":"2026-08-01T10:00:00Z"}' $BASE/api/v1/transactions)"
check "bad currency -> 400"    400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST -d '{"direction":"out","amount":"5","currency":"egp","occurred_at":"2026-08-01T10:00:00Z"}' $BASE/api/v1/transactions)"
check "future date -> 400"     400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST -d '{"direction":"out","amount":"5","currency":"EGP","occurred_at":"2027-08-01T10:00:00Z"}' $BASE/api/v1/transactions)"

echo "=== optimistic concurrency ==="
check "PATCH without If-Match -> 428" 428 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X PATCH -d '{"category":"X"}' $BASE/api/v1/transactions/$TXID)"
check "PATCH with stale version -> 409" 409 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -H "If-Match: 99" -X PATCH -d '{"category":"X"}' $BASE/api/v1/transactions/$TXID)"
check "PATCH with correct version -> 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -H "If-Match: 1" -X PATCH -d '{"category":"Repairs","counterparty":"Corrected Name"}' $BASE/api/v1/transactions/$TXID)"

echo "=== override read model ==="
AFTER=$(curl -s -H "$AUTH" $BASE/api/v1/transactions/$TXID)
echo "$AFTER" | python3 -c '
import json,sys
t=json.load(sys.stdin)
print("  effective counterparty:", t["counterparty"])
print("  parsed counterparty   :", t["parsed"]["counterparty"])
print("  category              :", t["category"])
print("  has_overrides         :", t["has_overrides"])
'
check "history has an entry" "yes" "$(curl -s -H "$AUTH" $BASE/api/v1/transactions/$TXID/history | python3 -c 'import json,sys; print("yes" if len(json.load(sys.stdin))>0 else "no")')"

echo "=== notes ==="
NOTE=$(curl -s -H "$AUTH" -H "$JSON" -X POST -d '{"body":"a note that must survive reparse"}' $BASE/api/v1/transactions/$TXID/notes)
check "note created" "yes" "$(echo "$NOTE" | python3 -c 'import json,sys; print("yes" if "id" in json.load(sys.stdin) else "no")' 2>/dev/null)"
check "blank note -> 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST -d '{"body":"   "}' $BASE/api/v1/transactions/$TXID/notes)"

echo
echo "=== SNAPSHOT BEFORE REPARSE ==="
BEFORE=$(curl -s -H "$AUTH" $BASE/api/v1/transactions/$TXID)
echo "$BEFORE" | python3 -c 'import json,sys; t=json.load(sys.stdin); print(json.dumps({k:t[k] for k in ("id","source","amount","currency","counterparty","category","description")}, ensure_ascii=False))'

echo "=== POST /admin/reparse (scope=all) ==="
curl -s -H "$AUTH" -H "$JSON" -X POST -d '{"scope":"all"}' $BASE/api/v1/admin/reparse | python3 -c 'import json,sys; print("  ", json.dumps(json.load(sys.stdin)))'

echo "=== SNAPSHOT AFTER REPARSE ==="
AFTER2=$(curl -s -H "$AUTH" $BASE/api/v1/transactions/$TXID)
echo "$AFTER2" | python3 -c 'import json,sys; t=json.load(sys.stdin); print(json.dumps({k:t[k] for k in ("id","source","amount","currency","counterparty","category","description")}, ensure_ascii=False))'

B=$(echo "$BEFORE"  | python3 -c 'import json,sys; t=json.load(sys.stdin); print(json.dumps({k:t[k] for k in ("amount","currency","counterparty","category","description","source")},sort_keys=True,ensure_ascii=False))')
A=$(echo "$AFTER2"  | python3 -c 'import json,sys; t=json.load(sys.stdin); print(json.dumps({k:t[k] for k in ("amount","currency","counterparty","category","description","source")},sort_keys=True,ensure_ascii=False))')
check "MANUAL ROW UNCHANGED BY FULL REPARSE" "$B" "$A"
check "note survived reparse" "1" "$(curl -s -H "$AUTH" $BASE/api/v1/transactions/$TXID/notes | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"

echo "=== soft delete ==="
V=$(echo "$AFTER2" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')
check "DELETE -> 204" 204 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "If-Match: $V" -X DELETE $BASE/api/v1/transactions/$TXID)"
check "deleted row -> 404" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/api/v1/transactions/$TXID)"

echo
echo "================================"
echo "  PASSED: $pass   FAILED: $fail"
echo "================================"
[ "$fail" -eq 0 ]
