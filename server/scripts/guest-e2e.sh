#!/bin/zsh
# Guest mode: owner shares a folder from a server with --public-url; two headless guests join
# with tickets only (rw and ro), no server of their own. Needs a debug build.
set -u
S=${REPLICA_E2E_DIR:-$(mktemp -d)}
BIN=${REPLICA_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/replica-server}
PORT=${REPLICA_E2E_PORT:-8796}
FAIL=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; FAIL=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi }
waitfor() { local n=$(( $1 * 2 )); while [ $n -gt 0 ]; do if eval "$2"; then return 0; fi; sleep 0.5; n=$((n-1)); done; return 1; }
pkill -f "replica-server --data-dir $S/gdata"; pkill -f "replica-server sync --ticket"; sleep 0.3
rm -rf "${S:?}/gdata" "${S:?}/O" "${S:?}/G1" "${S:?}/G2"; mkdir -p $S/O/Projects/X/sub $S/G1 $S/G2; : > $S/guest-server.log
U=http://127.0.0.1:$PORT
start() { ($BIN --data-dir $S/gdata serve --bind 127.0.0.1:$PORT --no-p2p --public-url $U >> $S/guest-server.log 2>&1 &); }
start; sleep 1.5
OWNER=$(grep -o 'rp_[0-9a-f]*' $S/guest-server.log | head -1)
api() { curl -s -m 10 -H "Authorization: Bearer $1" -H "Content-Type: application/json" "${@:2}"; }
j() { python3 -c "import sys,json; d=json.load(sys.stdin); $1"; }
echo "a-content" > $S/O/Projects/X/a.md; echo "b-content" > $S/O/Projects/X/sub/b.md; echo "secret" > $S/O/private.md; printf '\x89PNG fake' > $S/O/Projects/X/pic.png
(RUST_LOG=info $BIN sync --server $U --token $OWNER --vault va --dir $S/O --device O > $S/sO.log 2>&1 &); sleep 3

echo "== share + tickets"
R=$(api $OWNER -X POST $U/api/vaults/va/shares -d '{"path":"Projects/X","name":"X"}')
TICKET=$(echo "$R" | j 'print(d.get("ticket",""))'); SHARE=$(echo "$R" | j 'print(d.get("share",{}).get("id",""))')
check "share created" '[ -n "$SHARE" ]'
RO=$(api $OWNER -X POST $U/api/shares/$SHARE/tickets -d '{"label":"guest-ro","access":"ro"}' | j 'print(d["ticket"])')
INSPECT=$(api $OWNER -X POST $U/api/tickets/inspect -d "{\"ticket\":\"$TICKET\"}")
check "ticket carries the public url" '[ "$(echo "$INSPECT" | j "print(d[\"url\"])")" = "$U" ]'

echo "== guest auth hygiene"
C=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/guest/shares/$SHARE?secret=$(printf 'ab%.0s' {1..32})")
check "wrong secret -> 403" '[ "$C" = "403" ]'
C=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/guest/shares/doesnotexist?secret=$(printf 'ab%.0s' {1..32})")
check "unknown share -> same 403" '[ "$C" = "403" ]'
for i in {1..9}; do curl -s -m 5 -o /dev/null "$U/guest/shares/$SHARE?secret=$(printf 'cd%.0s' {1..32})"; done
C=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/guest/shares/$SHARE?secret=$(printf 'cd%.0s' {1..32})")
check "11th failure from one ip -> 429" '[ "$C" = "429" ]'
# the limiter is per ip; the real guests below come from the same loopback ip, so restart to clear it
pkill -f "replica-server --data-dir $S/gdata"; sleep 1; start; sleep 2
check "server log never contains a secret" '! grep -q "secret=" $S/guest-server.log'

echo "== guests join with tickets only"
(RUST_LOG=info $BIN sync --ticket $TICKET --dir $S/G1 --device G1 > $S/sG1.log 2>&1 &)
(RUST_LOG=info $BIN sync --ticket $RO --dir $S/G2 --device G2 > $S/sG2.log 2>&1 &)
waitfor 15 '[ -f $S/G1/a.md ] && [ -f $S/G1/pic.png ] && [ -f $S/G2/a.md ]'
check "rw guest got a.md" '[ "$(cat $S/G1/a.md 2>/dev/null)" = "a-content" ]'
check "rw guest got sub/b.md" '[ -f $S/G1/sub/b.md ]'
check "rw guest got the blob" 'cmp -s $S/O/Projects/X/pic.png $S/G1/pic.png'
check "guest never sees private.md" '[ ! -f $S/G1/private.md ]'
check "ro guest got a.md" '[ -f $S/G2/a.md ]'
LS=$(api $OWNER $U/api/vaults/va/sharing | j 'print([g.get("last_seen_at") is not None for g in d["shares"][0]["grants"]])')
check "owner sees tickets as used (last_seen_at)" '[ "$LS" = "[True, True]" ]'

echo "== edits"
echo "edited-by-guest" >> $S/G1/a.md; waitfor 10 'grep -q edited-by-guest $S/O/Projects/X/a.md'
check "owner sees guest edit" 'grep -q edited-by-guest $S/O/Projects/X/a.md'
echo "edited-by-owner" >> $S/O/Projects/X/sub/b.md; waitfor 10 'grep -q edited-by-owner $S/G1/sub/b.md && grep -q edited-by-owner $S/G2/sub/b.md'
check "both guests see owner edit" 'grep -q edited-by-owner $S/G1/sub/b.md && grep -q edited-by-owner $S/G2/sub/b.md'
echo "from guest" > $S/G1/new.md; printf 'GIF89a guest' > $S/G1/guest.gif
waitfor 15 '[ -f $S/O/Projects/X/new.md ] && [ -f $S/O/Projects/X/guest.gif ]'
check "owner got guest's new note" '[ "$(cat $S/O/Projects/X/new.md 2>/dev/null)" = "from guest" ]'
check "owner got guest's blob" 'cmp -s $S/G1/guest.gif $S/O/Projects/X/guest.gif'
waitfor 10 '[ -f $S/G2/guest.gif ]'; check "ro guest got the other guest's blob" 'cmp -s $S/G1/guest.gif $S/G2/guest.gif'

echo "== read-only guest"
waitfor 25 '[ ! -w $S/G2/a.md ]'
check "ro guest files are read-only on disk" '[ ! -w $S/G2/a.md ]'
chmod u+w $S/G2/a.md; echo "ro-edit" >> $S/G2/a.md; sleep 4
check "ro guest edit never reaches owner" '! grep -q ro-edit $S/O/Projects/X/a.md'
check "ro guest edit restored locally" '! grep -q ro-edit $S/G2/a.md'
echo "mine" > $S/G2/mine.md; sleep 4
check "ro guest's new file moved aside" '[ "$(cat "$S/G2/read-only copy/mine.md" 2>/dev/null)" = "mine" ] && [ ! -f $S/G2/mine.md ]'

echo "== revoke the ro ticket: guest is cut off and exits"
api $OWNER -X DELETE $U/api/shares/$SHARE/grants/guest-ro -o /dev/null -w "revoke -> %{http_code}\n"
waitfor 45 'grep -q "refused the connection" $S/sG2.log'
check "ro guest got cut off and stopped" 'grep -q "refused the connection" $S/sG2.log'
echo "after-revoke" >> $S/O/Projects/X/a.md; waitfor 10 'grep -q after-revoke $S/G1/a.md'
check "rw guest still receives" 'grep -q after-revoke $S/G1/a.md'
check "revoked guest does not" '! grep -q after-revoke $S/G2/a.md'

pkill -f "replica-server sync"; pkill -f "replica-server --data-dir $S/gdata"
echo "== server errors:"; grep -iE " error|panick" $S/guest-server.log | head -5
if [ $FAIL = 0 ]; then echo "ALL OK ($S)"; else echo "FAILURES ($S)"; fi
exit $FAIL
