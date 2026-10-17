// End-to-end: two clients sync through samsyn. Usage: node tests/e2e.mjs <ws-base> <token>
import * as Y from "yjs";
import { WebsocketProvider } from "y-websocket";
import WebSocket from "ws";

const [,, wsBase, token] = process.argv;
const vault = "e2e-" + Date.now();

function client(name) {
  const doc = new Y.Doc();
  const p = new WebsocketProvider(wsBase, vault, doc, { params: { token }, WebSocketPolyfill: WebSocket, disableBc: true });
  p.awareness.setLocalStateField("user", { name });
  return { doc, p, texts: doc.getMap("texts") };
}
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, ms = 5000) {
  const t0 = Date.now();
  while (!fn()) { if (Date.now() - t0 > ms) throw new Error("timeout"); await wait(25); }
}

const a = client("A");
await until(() => a.p.synced);
a.doc.transact(() => a.texts.set("note.md", new Y.Text("hello")));

const b = client("B");
await until(() => b.p.synced && b.texts.get("note.md")?.toString() === "hello");
console.log("B received initial state:", b.texts.get("note.md").toString());

// Concurrent live edits.
a.texts.get("note.md").insert(5, " world");
b.texts.get("note.md").insert(0, "Oh, ");
await until(() => a.texts.get("note.md").toString() === b.texts.get("note.md").toString() && a.texts.get("note.md").toString().length === 15);
console.log("converged:", JSON.stringify(a.texts.get("note.md").toString()));

await until(() => a.p.awareness.getStates().size === 2);
console.log("awareness peers seen by A:", a.p.awareness.getStates().size);

a.p.destroy(); b.p.destroy();
console.log("VAULT=" + vault);
process.exit(0);
