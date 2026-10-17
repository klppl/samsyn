#!/bin/zsh
# Server-to-server sharing scenario: two servers on one machine linked over iroh
# (direct connection, relays disabled), headless clients standing in for plugins.
# Usage: server/scripts/remote-e2e.sh   (needs a debug build: cargo build)
set -u
S=${SAMSYN_E2E_DIR:-$(mktemp -d)}
BIN=${SAMSYN_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/samsyn}
PA=${SAMSYN_E2E_PORT_A:-8798}; PB=${SAMSYN_E2E_PORT_B:-8799}
RELAY=${SAMSYN_E2E_RELAY:-none}   # "none" = direct only, "default" = n0 public relays, or a relay URL
RELAYFLAG=(--relay $RELAY); [ "$RELAY" = default ] && RELAYFLAG=()
FAIL=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; FAIL=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi }
waitfor() { # waitfor <seconds> <cond>
  local n=$(( $1 * 2 )); while [ $n -gt 0 ]; do if eval "$2"; then return 0; fi; sleep 0.5; n=$((n-1)); done; return 1; }
pkill -f "samsyn" ; sleep 0.5
rm -rf "${S:?}/sA" "${S:?}/sB" "${S:?}/A" "${S:?}/B" "${S:?}/C"; mkdir -p $S/A/Projects/X/sub $S/B $S/C; : > $S/serverA.log; : > $S/serverB.log
startA() { ($BIN --data-dir $S/sA serve --bind 127.0.0.1:$PA $RELAYFLAG --p2p-port $((PA+1000)) >> $S/serverA.log 2>&1 &); }
startB() { ($BIN --data-dir $S/sB serve --bind 127.0.0.1:$PB $RELAYFLAG --p2p-port $((PB+1000)) >> $S/serverB.log 2>&1 &); }
startA; startB; sleep 3
OWNER=$(grep -o 'rp_[0-9a-f]*' $S/serverA.log | head -1)
BOB=$(grep -o 'rp_[0-9a-f]*' $S/serverB.log | head -1)
CAROL=$($BIN --data-dir $S/sB user add carol | grep -o 'rp_[0-9a-f]*')
UA=http://127.0.0.1:$PA; UB=http://127.0.0.1:$PB
api() { curl -s -m 40 -H "Authorization: Bearer $1" -H "Content-Type: application/json" "${@:2}"; }
j() { python3 -c "import sys,json; d=json.load(sys.stdin); $1"; }
NA=$(curl -s $UA/health | j 'print(d["node_id"])'); NB=$(curl -s $UB/health | j 'print(d["node_id"])')
check "two distinct node ids" '[ -n "$NA" ] && [ -n "$NB" ] && [ "$NA" != "$NB" ]'
echo "a-content" > $S/A/Projects/X/a.md; echo "b-content" > $S/A/Projects/X/sub/b.md; echo "secret" > $S/A/private.md; printf '\x89PNG fake' > $S/A/Projects/X/pic.png
(RUST_LOG=info $BIN sync --server $UA --token $OWNER --vault va --dir $S/A --device A > $S/sA.log 2>&1 &)
(RUST_LOG=info $BIN sync --server $UB --token $BOB --vault vb --dir $S/B --device B > $S/sB.log 2>&1 &); sleep 3

echo "== owner (server A) creates share"
R=$(api $OWNER -X POST $UA/api/vaults/va/shares -d '{"path":"Projects/X","name":"X"}')
TICKET=$(echo "$R" | j 'print(d.get("ticket",""))'); SHARE=$(echo "$R" | j 'print(d.get("share",{}).get("id",""))')
check "share created" '[ -n "$SHARE" ]'
echo "ticket: ${TICKET:0:40}…"

echo "== bad ticket is refused fast by B"
BAD=$(api $BOB -X POST $UB/api/vaults/vb/mounts -d "{\"ticket\":\"samsynxxxx\",\"mount_path\":\"Shared/X\"}" -o /dev/null -w '%{http_code}')
check "garbage ticket -> 400" '[ "$BAD" = "400" ]'

echo "== bob (server B) mounts over p2p"
T0=$(date +%s)
R=$(api $BOB -X POST $UB/api/vaults/vb/mounts -d "{\"ticket\":\"$TICKET\",\"mount_path\":\"Shared/X\"}")
echo "$R" | j 'print(d.get("error") or ("mounted at", d["mount_path"], d["access"]))'
echo "mount took $(( $(date +%s) - T0 ))s"
check "mount accepted" '[ "$(echo "$R" | j "print(d.get(\"access\",\"\"))")" = "rw" ]'
waitfor 20 '[ -f $S/B/Shared/X/a.md ] && [ -f $S/B/Shared/X/pic.png ]'
check "bob got a.md" '[ "$(cat $S/B/Shared/X/a.md 2>/dev/null)" = "a-content" ]'
check "bob got sub/b.md" '[ -f $S/B/Shared/X/sub/b.md ]'
check "private.md not shared" '[ ! -f $S/B/private.md ]'
check "blob arrived over p2p" 'cmp -s $S/A/Projects/X/pic.png $S/B/Shared/X/pic.png'
ST=$(api $BOB $UB/api/vaults/vb/sharing | j 'print(d["mounts"][0]["status"])'); echo "mount status: $ST"
check "link status connected" '[ "$ST" = "remote: connected" ]'

echo "== edit B -> A"; echo "edited-by-bob" >> $S/B/Shared/X/a.md
waitfor 10 'grep -q edited-by-bob $S/A/Projects/X/a.md'; check "owner sees bob edit" 'grep -q edited-by-bob $S/A/Projects/X/a.md'
echo "== edit A -> B"; echo "edited-by-owner" >> $S/A/Projects/X/sub/b.md
waitfor 10 'grep -q edited-by-owner $S/B/Shared/X/sub/b.md'; check "bob sees owner edit" 'grep -q edited-by-owner $S/B/Shared/X/sub/b.md'
echo "== concurrent"
{ printf 'TOP\n' | cat - $S/A/Projects/X/a.md > $S/A/.tmp && mv $S/A/.tmp $S/A/Projects/X/a.md; } &
echo "BOTTOM" >> $S/B/Shared/X/a.md; wait; sleep 4
check "concurrent edits merged both sides" '[ "$(cat $S/A/Projects/X/a.md)" = "$(cat $S/B/Shared/X/a.md)" ] && grep -q TOP $S/A/Projects/X/a.md && grep -q BOTTOM $S/A/Projects/X/a.md'
echo "== new file + blob from B, delete from A"
echo "from bob" > $S/B/Shared/X/new.md; printf 'GIF89a bob' > $S/B/Shared/X/bob.gif
waitfor 15 '[ -f $S/A/Projects/X/new.md ] && [ -f $S/A/Projects/X/bob.gif ]'
check "owner got bob's new.md" '[ "$(cat $S/A/Projects/X/new.md 2>/dev/null)" = "from bob" ]'
check "owner got bob's blob over p2p" 'cmp -s $S/B/Shared/X/bob.gif $S/A/Projects/X/bob.gif'
rm $S/A/Projects/X/sub/b.md; waitfor 10 '[ ! -f $S/B/Shared/X/sub/b.md ]'
check "owner delete reached bob" '[ ! -f $S/B/Shared/X/sub/b.md ]'

echo "== read-only ticket, carol on server B"
RO=$(api $OWNER -X POST $UA/api/shares/$SHARE/tickets -d '{"label":"carol","access":"ro"}' | j 'print(d["ticket"])')
(RUST_LOG=info $BIN sync --server $UB --token $CAROL --vault vc --dir $S/C --device C > $S/sC.log 2>&1 &); sleep 2
api $CAROL -X POST $UB/api/vaults/vc/mounts -d "{\"ticket\":\"$RO\"}" | j 'print(d.get("error") or ("carol mounted at", d["mount_path"], d["access"]))'
waitfor 20 '[ -f $S/C/Shared/X/a.md ] && [ -f $S/C/Shared/X/pic.png ]'
check "carol got a.md" 'grep -q TOP $S/C/Shared/X/a.md'
check "carol got blob" 'cmp -s $S/A/Projects/X/pic.png $S/C/Shared/X/pic.png'
echo "carol-edit" >> $S/C/Shared/X/a.md; sleep 4
check "carol edit not in owner" '! grep -q carol-edit $S/A/Projects/X/a.md'
check "carol edit reverted locally" '! grep -q carol-edit $S/C/Shared/X/a.md'

echo "== revoke carol on A -> B link denied, mount revoked"
api $OWNER -X DELETE $UA/api/shares/$SHARE/grants/carol -o /dev/null -w "revoke -> %{http_code}\n"
waitfor 60 '[ "$(api $CAROL $UB/api/vaults/vc/sharing | j "print(d[\"mounts\"][0][\"status\"])")" = "revoked" ]'
check "carol mount shows revoked on B" '[ "$(api $CAROL $UB/api/vaults/vc/sharing | j "print(d[\"mounts\"][0][\"status\"])")" = "revoked" ]'
echo "after-revoke" >> $S/A/Projects/X/a.md; waitfor 10 'grep -q after-revoke $S/B/Shared/X/a.md'
check "bob still receives" 'grep -q after-revoke $S/B/Shared/X/a.md'
check "carol stopped receiving" '! grep -q after-revoke $S/C/Shared/X/a.md'
check "carol keeps her files" '[ -f $S/C/Shared/X/a.md ]'

echo "== restart B: link reconnects"; pkill -f "samsyn --data-dir $S/sB"; sleep 1; startB; sleep 3
echo "while-b-down" >> $S/A/Projects/X/a.md
waitfor 30 'grep -q while-b-down $S/B/Shared/X/a.md'; check "edit flows after B restart" 'grep -q while-b-down $S/B/Shared/X/a.md'
echo "== restart A: link reconnects"; pkill -f "samsyn --data-dir $S/sA"; sleep 1
echo "while-a-down" >> $S/B/Shared/X/a.md; sleep 1; startA; sleep 3
waitfor 60 'grep -q while-a-down $S/A/Projects/X/a.md'; check "edit made while A was down arrives after A restart" 'grep -q while-a-down $S/A/Projects/X/a.md'
echo "after-a-restart" >> $S/A/Projects/X/a.md; waitfor 30 'grep -q after-a-restart $S/B/Shared/X/a.md'
check "edit flows after A restart" 'grep -q after-a-restart $S/B/Shared/X/a.md'

echo "== unmount bob (files stay)"; MID=$(api $BOB $UB/api/vaults/vb/sharing | j 'print(d["mounts"][0]["id"])'); api $BOB -X DELETE $UB/api/mounts/$MID -o /dev/null -w "unmount -> %{http_code}\n"
check "bob keeps files after unmount" '[ -f $S/B/Shared/X/a.md ]'
check "replica dir removed" '! ls -d $S/sB/users/*/mounts/$MID >/dev/null 2>&1'
pkill -f "samsyn"
echo "== server errors:"; grep -iE " error|panick" $S/serverA.log $S/serverB.log | grep -v 'p2p link failed' | head -5
if [ $FAIL = 0 ]; then echo "ALL OK ($S)"; else echo "FAILURES ($S)"; fi
exit $FAIL
