import "fake-indexeddb/auto";
import { test } from "node:test";
import assert from "node:assert/strict";
import * as Y from "yjs";
import { Persistence } from "../src/persistence";

/** Minimal in-memory stand-in for Obsidian's DataAdapter (only what Persistence uses). */
class FakeAdapter {
  files = new Map<string, string | ArrayBuffer>();
  async exists(p: string) {
    return this.files.has(p);
  }
  async read(p: string) {
    return this.files.get(p) as string;
  }
  async readBinary(p: string) {
    return this.files.get(p) as ArrayBuffer;
  }
  async write(p: string, s: string) {
    this.files.set(p, s);
  }
  async writeBinary(p: string, b: ArrayBuffer) {
    this.files.set(p, b);
  }
  async remove(p: string) {
    this.files.delete(p);
  }
}

const adapter = () => new FakeAdapter() as unknown as import("obsidian").DataAdapter;
let n = 0;
const scope = () => `test-${Date.now()}-${n++}`;

test("document and synced hashes survive close + reopen via IndexedDB", async () => {
  const a = adapter();
  const sc = scope();
  const p1 = new Persistence(a, "plug", sc);
  assert.equal(p1.backend, "indexeddb");
  const d1 = new Y.Doc();
  assert.equal(await p1.loadDoc(d1), false, "first run has no cached doc");
  const st = await p1.loadState("install-1");
  d1.getMap<Y.Text>("texts").set("a.md", new Y.Text("hello"));
  p1.noteSynced("a.md", "h1", st);
  p1.noteSynced("b.md", "h2", st);
  p1.noteForgotten("b.md", st);
  await p1.close();
  d1.destroy();

  const p2 = new Persistence(a, "plug", sc);
  const d2 = new Y.Doc();
  assert.equal(await p2.loadDoc(d2), true, "second run finds the cached doc");
  assert.equal(d2.getMap<Y.Text>("texts").get("a.md")?.toString(), "hello");
  const st2 = await p2.loadState("install-1");
  assert.deepEqual(st2.synced, { "a.md": "h1" });
  await p2.close();
  d2.destroy();
});

test("legacy doc.bin and state.json are migrated once and removed", async () => {
  const a = adapter();
  const sc = scope();
  const legacy = new Y.Doc();
  legacy.getMap<Y.Text>("texts").set("old.md", new Y.Text("from file"));
  const upd = Y.encodeStateAsUpdate(legacy);
  await a.writeBinary("plug/doc.bin", upd.buffer.slice(upd.byteOffset, upd.byteOffset + upd.byteLength) as ArrayBuffer);
  await a.write("plug/state.json", JSON.stringify({ installId: "install-1", synced: { "old.md": "h-old" } }));

  const p = new Persistence(a, "plug", sc);
  const d = new Y.Doc();
  const st = await p.loadState("install-1");
  assert.deepEqual(st.synced, { "old.md": "h-old" });
  assert.equal(await p.loadDoc(d), true);
  assert.equal(d.getMap<Y.Text>("texts").get("old.md")?.toString(), "from file");
  assert.equal(await a.exists("plug/doc.bin"), false, "doc.bin removed after migration");
  assert.equal(await a.exists("plug/state.json"), false, "state.json removed after migration");
  await p.close();
  d.destroy();

  const p2 = new Persistence(a, "plug", sc);
  const d2 = new Y.Doc();
  assert.equal(await p2.loadDoc(d2), true, "migrated doc persisted in IndexedDB");
  assert.equal(d2.getMap<Y.Text>("texts").get("old.md")?.toString(), "from file");
  await p2.close();
  d2.destroy();
});

test("state.json from another install is not migrated", async () => {
  const a = adapter();
  await a.write("plug/state.json", JSON.stringify({ installId: "other", synced: { "x.md": "h" } }));
  const p = new Persistence(a, "plug", scope());
  const st = await p.loadState("install-1");
  assert.deepEqual(st.synced, {});
  await p.close();
});

test("clear() wipes both databases", async () => {
  const a = adapter();
  const sc = scope();
  const p = new Persistence(a, "plug", sc);
  const d = new Y.Doc();
  await p.loadDoc(d);
  const st = await p.loadState("i");
  d.getMap<Y.Text>("texts").set("a.md", new Y.Text("x"));
  p.noteSynced("a.md", "h", st);
  await p.clear();
  d.destroy();

  const p2 = new Persistence(a, "plug", sc);
  const d2 = new Y.Doc();
  assert.equal(await p2.loadDoc(d2), false);
  assert.deepEqual((await p2.loadState("i")).synced, {});
  await p2.close();
  d2.destroy();
});

test("files fallback keeps working", async () => {
  const a = adapter();
  const p = new Persistence(a, "plug", scope(), true);
  assert.equal(p.backend, "files");
  const d = new Y.Doc();
  await p.loadDoc(d);
  const st = await p.loadState("i");
  d.getMap<Y.Text>("texts").set("a.md", new Y.Text("x"));
  p.noteSynced("a.md", "h", st);
  await p.close();
  assert.equal(await a.exists("plug/doc.bin"), true);
  assert.equal(await a.exists("plug/state.json"), true);
  d.destroy();
});
