#!/bin/zsh
# Sharing scenario on one server with headless clients standing in for plugins.
# Usage: server/scripts/share-e2e.sh   (needs a debug build: cargo build)
# Set REPLICA_E2E_DIR to keep the work dir; otherwise a temp dir is used.
set -u
S=${REPLICA_E2E_DIR:-$(mktemp -d)}
BIN=${REPLICA_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/replica-server}
PORT=${REPLICA_E2E_PORT:-8798}
FAIL=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; FAIL=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi }
pkill -f "replica-server" ; sleep 0.5
rm -rf "${S:?}/sdata" "${S:?}/A" "${S:?}/B" "${S:?}/C"; mkdir -p $S/A/Projects/X/sub $S/B $S/C
($BIN --data-dir $S/sdata serve --bind 127.0.0.1:$PORT > $S/share-server.log 2>&1 &); sleep 1.5
OWNER=$(grep -o 'rp_[0-9a-f]*' $S/share-server.log | head -1)
BOB=$($BIN --data-dir $S/sdata user add bob | grep -o 'rp_[0-9a-f]*')
CAROL=$($BIN --data-dir $S/sdata user add carol | grep -o 'rp_[0-9a-f]*')
U=http://127.0.0.1:$PORT
api() { curl -s -m 10 -H "Authorization: Bearer $1" -H "Content-Type: application/json" "${@:2}"; }
j() { python3 -c "import sys,json; d=json.load(sys.stdin); $1"; }
echo "a-content" > $S/A/Projects/X/a.md; echo "b-content" > $S/A/Projects/X/sub/b.md; echo "secret" > $S/A/private.md; printf '\x89PNG fake' > $S/A/Projects/X/pic.png
(RUST_LOG=info $BIN sync --server $U --token $OWNER --vault va --dir $S/A --device A > $S/sA.log 2>&1 &)
(RUST_LOG=info $BIN sync --server $U --token $BOB --vault vb --dir $S/B --device B > $S/sB.log 2>&1 &); sleep 3

echo "== create share"
R=$(api $OWNER -X POST $U/api/vaults/va/shares -d '{"path":"Projects/X","name":"X"}')
TICKET=$(echo "$R" | j 'print(d.get("ticket",""))'); SHARE=$(echo "$R" | j 'print(d.get("share",{}).get("id",""))')
check "share created" '[ -n "$SHARE" ]'

echo "== concurrent overlapping creates (only one may succeed)"
C1=$(api $OWNER -X POST $U/api/vaults/va/shares -d '{"path":"Projects","name":"P1"}' -o /dev/null -w '%{http_code}' &)
C2=$(api $OWNER -X POST $U/api/vaults/va/shares -d '{"path":"Projects","name":"P2"}' -o /dev/null -w '%{http_code}' &)
wait
# Both overlap the existing share Projects/X/, so both must be 409.
check "overlap check serialized (both 409)" '[ "$C1$C2" = "409409" ]'
N=$(api $OWNER $U/api/vaults/va/sharing | j 'print(len(d["shares"]))')
check "still exactly one share" '[ "$N" = "1" ]'

echo "== bob mounts"
api $BOB -X POST $U/api/vaults/vb/mounts -d "{\"ticket\":\"$TICKET\",\"mount_path\":\"Shared/X\"}" | j 'print(d.get("error") or ("mounted at", d["mount_path"], d["access"]))'; sleep 3
check "bob got a.md" '[ "$(cat $S/B/Shared/X/a.md)" = "a-content" ]'
check "bob got sub/b.md" '[ -f $S/B/Shared/X/sub/b.md ]'
check "private.md not shared" '[ ! -f $S/B/private.md ]'
check "blob arrived on mount" 'cmp -s $S/A/Projects/X/pic.png $S/B/Shared/X/pic.png'
check "no 'not present in source' blob log" '! grep -q "not present in source" $S/share-server.log'

echo "== edit B -> A"; echo "edited-by-bob" >> $S/B/Shared/X/a.md; sleep 2
check "owner sees bob edit" 'grep -q edited-by-bob $S/A/Projects/X/a.md'
echo "== edit A -> B"; echo "edited-by-owner" >> $S/A/Projects/X/sub/b.md; sleep 2
check "bob sees owner edit" 'grep -q edited-by-owner $S/B/Shared/X/sub/b.md'
echo "== concurrent"
{ printf 'TOP\n' | cat - $S/A/Projects/X/a.md > $S/A/.tmp && mv $S/A/.tmp $S/A/Projects/X/a.md; } &
echo "BOTTOM" >> $S/B/Shared/X/a.md; wait; sleep 3
check "concurrent edits merged both sides" '[ "$(cat $S/A/Projects/X/a.md)" = "$(cat $S/B/Shared/X/a.md)" ] && grep -q TOP $S/A/Projects/X/a.md && grep -q BOTTOM $S/A/Projects/X/a.md'
echo "== new file + new blob in B, delete in A"
echo "from bob" > $S/B/Shared/X/new.md; printf 'GIF89a bob' > $S/B/Shared/X/bob.gif; sleep 3
check "owner got bob's new.md" '[ "$(cat $S/A/Projects/X/new.md 2>/dev/null)" = "from bob" ]'
check "owner got bob's blob" 'cmp -s $S/B/Shared/X/bob.gif $S/A/Projects/X/bob.gif'
rm $S/A/Projects/X/sub/b.md; sleep 2
check "owner delete reached bob" '[ ! -f $S/B/Shared/X/sub/b.md ]'

echo "== read-only ticket for carol"
RO=$(api $OWNER -X POST $U/api/shares/$SHARE/tickets -d '{"label":"carol","access":"ro"}' | j 'print(d["ticket"])')
(RUST_LOG=info $BIN sync --server $U --token $CAROL --vault vc --dir $S/C --device C > $S/sC.log 2>&1 &); sleep 2
api $CAROL -X POST $U/api/vaults/vc/mounts -d "{\"ticket\":\"$RO\"}" | j 'print(d.get("error") or ("carol mounted at", d["mount_path"], d["access"]))'; sleep 3
check "carol got a.md" 'grep -q TOP $S/C/Shared/X/a.md'
check "carol got blob" 'cmp -s $S/A/Projects/X/pic.png $S/C/Shared/X/pic.png'
echo "carol-edit" >> $S/C/Shared/X/a.md; sleep 3
check "carol edit not in owner" '! grep -q carol-edit $S/A/Projects/X/a.md'
check "carol edit reverted locally" '! grep -q carol-edit $S/C/Shared/X/a.md'
echo "== carol creates a file on the ro mount: kept as read-only copy"
echo "carol-new" > $S/C/Shared/X/carol.md; printf 'BM carol' > $S/C/Shared/X/carol.bmp; sleep 4
check "carol.md removed from mount" '[ ! -f $S/C/Shared/X/carol.md ]'
check "carol.md rescued" '[ "$(cat "$S/C/Shared/X (read-only copy)/carol.md" 2>/dev/null)" = "carol-new" ]'
check "carol.bmp rescued" '[ "$(cat "$S/C/Shared/X (read-only copy)/carol.bmp" 2>/dev/null)" = "BM carol" ]'
check "carol.md never reached owner" '[ ! -f $S/A/Projects/X/carol.md ]'

echo "== listing"; api $OWNER $U/api/vaults/va/sharing | j 's=d["shares"][0]; print("share", s["name"], s["path"], "grants", [(g["label"],g["access"]) for g in s["grants"]], "mounted_by", s["mounted_by"])'
echo "== revoke carol"; api $OWNER -X DELETE $U/api/shares/$SHARE/grants/carol -o /dev/null -w "revoke -> %{http_code}\n"
echo "after-revoke" >> $S/A/Projects/X/a.md; sleep 2
check "bob still receives after carol revoke" 'grep -q after-revoke $S/B/Shared/X/a.md'
check "carol stopped receiving" '! grep -q after-revoke $S/C/Shared/X/a.md'
echo "== restart server, bridges reattach"; pkill -f "replica-server --data-dir"; sleep 1
($BIN --data-dir $S/sdata serve --bind 127.0.0.1:$PORT >> $S/share-server.log 2>&1 &); sleep 4
echo "after-restart" >> $S/B/Shared/X/a.md; sleep 3
check "edit flows after restart" 'grep -q after-restart $S/A/Projects/X/a.md'
echo "== unmount bob (files stay)"; MID=$(api $BOB $U/api/vaults/vb/sharing | j 'print(d["mounts"][0]["id"])'); api $BOB -X DELETE $U/api/mounts/$MID -o /dev/null -w "unmount -> %{http_code}\n"
check "bob keeps files after unmount" '[ -f $S/B/Shared/X/a.md ]'
pkill -f "replica-server"
echo "== server errors:"; grep -iE " error|panick" $S/share-server.log | head -5
if [ $FAIL = 0 ]; then echo "ALL OK ($S)"; else echo "FAILURES ($S)"; fi
exit $FAIL
