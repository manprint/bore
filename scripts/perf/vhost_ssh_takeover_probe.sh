#!/usr/bin/env bash
# S7b: full state dump around a same-identity SSH takeover of a vhost label.
# The server registered TWO admin rows for one label while the admin vhost list
# reported one, and the incumbent kept serving. Find out what the real state is.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
OP=5052; OUT=$H/out; GW=${BORE_HOST}; GWP=443
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
rows(){ adm vhost | jq -c --arg l "$1" '[.[]|select(.subdomain==$l)]'; }
trows(){ adm tunnels | jq -c --arg l "$1" '[.[]|select((.secret_id//"")==$l)]'; }
code(){ curl -s -o /dev/null -m 20 -w '%{http_code}/%{time_total}' "https://$1.$GW/ping"; }
SSHOPT=(-T -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
        -o PubkeyAuthentication=no -o PreferredAuthentications=password
        -o ExitOnForwardFailure=yes -o LogLevel=DEBUG1)
curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping || { echo "origin down"; exit 1; }
# a second, distinguishable origin: it answers /ping with a different body length
python3 - 5054 > $OUT/o5054.log 2>&1 <<'PY' &
import socket,sys,threading
p=int(sys.argv[1]) if len(sys.argv)>1 else 5054
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(("127.0.0.1",p)); s.listen(64)
def h(c):
    try:
        c.recv(65536)
        b=b"CHALLENGER"
        c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: keep-alive\r\n\r\n%s"%(len(b),b))
    except Exception: pass
    finally:
        try: c.close()
        except Exception: pass
while True:
    c,_=s.accept(); threading.Thread(target=h,args=(c,),daemon=True).start()
PY
O2=$!; KIDS+=("$O2"); sleep 1
echo "challenger origin says: $(curl -s -m 3 http://127.0.0.1:5054/ping)"

L="s7b$(date +%s | cut -c6-10)"
echo "label=$L"
sshpass -p "$SSHGW_PASS" ssh "${SSHOPT[@]}" -R "vhost/$L:80:127.0.0.1:$OP" -p $GWP "$SSHGW_USER@$GW" \
  > $OUT/$L-a.ssh 2>&1 < /dev/null & A=$!; KIDS+=("$A")
for i in $(seq 40); do [ "$(rows "$L")" != "[]" ] && break; sleep 0.5; done
echo "--- incumbent only ---"
echo "  vhost rows:   $(rows "$L")"
echo "  tunnel rows:  $(trows "$L")"
echo "  body:         $(curl -s -m 20 https://$L.$GW/ping | head -c 40)"

echo "--- challenger joins (same identity, DIFFERENT origin on 5054) ---"
sshpass -p "$SSHGW_PASS" ssh "${SSHOPT[@]}" -R "vhost/$L:80:127.0.0.1:5054" -p $GWP "$SSHGW_USER@$GW" \
  > $OUT/$L-b.ssh 2>&1 < /dev/null & Bp=$!; KIDS+=("$Bp")
for t in 3 8 20; do
  sleep $((t == 3 ? 3 : 5))
  echo "  t+${t}s rows=$(rows "$L")"
  echo "        body='$(curl -s -m 20 https://$L.$GW/ping | head -c 40)' code=$(code "$L")"
done
echo "  incumbent alive=$(kill -0 $A 2>/dev/null && echo yes || echo no)  challenger alive=$(kill -0 $Bp 2>/dev/null && echo yes || echo no)"
echo "  challenger banner/errors:"; grep -viE '^\s*$' $OUT/$L-b.ssh | tail -14 | sed 's/^/      /'
echo "  incumbent stderr tail:";    grep -iE 'forward|error|closed|remote' $OUT/$L-a.ssh | tail -6 | sed 's/^/      /'

echo "--- kill the challenger only ---"
kill -9 $Bp 2>/dev/null; sleep 5
echo "  rows=$(rows "$L")  body='$(curl -s -m 20 https://$L.$GW/ping | head -c 40)'"
echo "--- kill the incumbent too ---"
kill -9 $A 2>/dev/null
for i in $(seq 20); do [ "$(rows "$L")" = "[]" ] && { echo "  released after $((i*2))s"; break; }; sleep 2; done
[ "$(rows "$L")" = "[]" ] || echo "  *** STILL REGISTERED: $(rows "$L")"
kill -9 $O2 2>/dev/null
echo DONE
