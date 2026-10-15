#!/bin/zsh
# One server, owner shares Projects/X, bob mounts at Shared/X, then presence-e2e.mjs checks that
# awareness crosses the bridge. Needs a debug build and plugin/node_modules (npm install in plugin/).
set -u
S=${REPLICA_E2E_DIR:-$(mktemp -d)}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${REPLICA_BIN:-$ROOT/server/target/debug/replica-server}
PORT=${REPLICA_E2E_PORT:-8797}
pkill -f "replica-server --data-dir $S/pdata" ; sleep 0.3
rm -rf "${S:?}/pdata" "${S:?}/PA"; mkdir -p $S/PA/Projects/X/sub
($BIN --data-dir $S/pdata serve --bind 127.0.0.1:$PORT --no-p2p > $S/presence-server.log 2>&1 &); sleep 1.5
OWNER=$(grep -o 'rp_[0-9a-f]*' $S/presence-server.log | head -1)
BOB=$($BIN --data-dir $S/pdata user add bob | grep -o 'rp_[0-9a-f]*')
U=http://127.0.0.1:$PORT
api() { curl -s -m 10 -H "Authorization: Bearer $1" -H "Content-Type: application/json" "${@:2}"; }
j() { python3 -c "import sys,json; d=json.load(sys.stdin); $1"; }
echo "a" > $S/PA/Projects/X/a.md; echo "b" > $S/PA/Projects/X/sub/b.md
$BIN sync --server $U --token $OWNER --vault va --dir $S/PA --device A --once > $S/pA.log 2>&1
# bob's vault must exist before mounting: touch it through /ws by a once-sync of an empty dir
mkdir -p $S/PB; $BIN sync --server $U --token $BOB --vault vb --dir $S/PB --device B --once > $S/pB.log 2>&1
TICKET=$(api $OWNER -X POST $U/api/vaults/va/shares -d '{"path":"Projects/X","name":"X"}' | j 'print(d["ticket"])')
api $BOB -X POST $U/api/vaults/vb/mounts -d "{\"ticket\":\"$TICKET\",\"mount_path\":\"Shared/X\"}" | j 'print(d.get("error") or "mounted")'
cd $ROOT/plugin && node tests/presence-e2e.mjs ws://127.0.0.1:$PORT/ws $OWNER va $BOB vb
RC=$?
pkill -f "replica-server --data-dir $S/pdata"
exit $RC
