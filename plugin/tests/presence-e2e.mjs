// Presence across a bridge: owner's client sets awareness file=Projects/X/a.md; bob's client on
// the mounting vault must see a mirrored peer with file=Shared/X/a.md and _via set; moving the
// owner out of the shared folder removes the mirror. Run from plugin/ (needs its node_modules):
//   node tests/presence-e2e.mjs <ws-base> <owner-token> <owner-vault> <bob-token> <bob-vault>
import * as Y from "yjs";
import { WebsocketProvider } from "y-websocket";
import WebSocket from "ws";

const [, , wsBase, ownerToken, ownerVault, bobToken, bobVault] = process.argv;
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, what, ms = 8000) {
  const t0 = Date.now();
  while (!fn()) {
    if (Date.now() - t0 > ms) throw new Error("timeout waiting for: " + what);
    await wait(50);
  }
}
function client(name, vault, token) {
  const doc = new Y.Doc();
  const p = new WebsocketProvider(wsBase, vault, doc, { params: { token }, WebSocketPolyfill: WebSocket, disableBc: true });
  p.awareness.setLocalStateField("user", { name });
  return { doc, p };
}
const peers = (c) => [...c.p.awareness.getStates().entries()].filter(([id]) => id !== c.doc.clientID).map(([, s]) => s);
const mirrors = (c) => peers(c).filter((s) => s && s._via);

const anna = client("Anna", ownerVault, ownerToken);
const bob = client("Bob", bobVault, bobToken);
await until(() => anna.p.synced && bob.p.synced, "both synced");

let fail = 0;
const check = (label, ok) => { console.log((ok ? "ok   " : "FAIL ") + label); if (!ok) fail = 1; };

// 1. Anna edits inside the shared folder → Bob sees a mirrored peer at the mapped path.
anna.p.awareness.setLocalStateField("file", "Projects/X/a.md");
try {
  await until(() => mirrors(bob).some((s) => s.file === "Shared/X/a.md" && s.user?.name === "Anna"), "mirror of Anna at Bob");
  check("bob sees Anna in Shared/X/a.md", true);
} catch (e) { check("bob sees Anna in Shared/X/a.md (" + e.message + ")", false); }

// 2. Bob edits inside the mount → Anna sees Bob at Projects/X/sub/b.md.
bob.p.awareness.setLocalStateField("file", "Shared/X/sub/b.md");
try {
  await until(() => mirrors(anna).some((s) => s.file === "Projects/X/sub/b.md" && s.user?.name === "Bob"), "mirror of Bob at Anna");
  check("anna sees Bob in Projects/X/sub/b.md", true);
} catch (e) { check("anna sees Bob in Projects/X/sub/b.md (" + e.message + ")", false); }

// 3. Anna moves to a private note → her mirror disappears for Bob.
anna.p.awareness.setLocalStateField("file", "private.md");
try {
  await until(() => !mirrors(bob).some((s) => s.user?.name === "Anna"), "Anna's mirror removed");
  check("anna leaving the folder removes her mirror", true);
} catch (e) { check("anna leaving the folder removes her mirror (" + e.message + ")", false); }
check("bob never sees Anna's private path", !peers(bob).some((s) => s && s.file === "private.md"));

// 4. Bob disconnects → Anna's mirror of Bob disappears.
bob.p.destroy();
try {
  await until(() => !mirrors(anna).some((s) => s.user?.name === "Bob"), "Bob's mirror removed", 10000);
  check("bob disconnecting removes his mirror", true);
} catch (e) { check("bob disconnecting removes his mirror (" + e.message + ")", false); }

anna.p.destroy();
console.log(fail ? "PRESENCE FAILURES" : "PRESENCE OK");
process.exit(fail);
