import { test } from "node:test";
import assert from "node:assert/strict";
import * as Y from "yjs";
import { applyDiff, diffText } from "../src/diff";
import { hashText } from "../src/hash";

const cases: Array<[string, string]> = [
  ["", "hello"],
  ["hello", ""],
  ["hello", "hello"],
  ["hello world", "hello there world"],
  ["abc", "abd"],
  ["aaa", "aa"],
  ["aa", "aaa"],
  ["x😀y", "x😀😀y"],
  ["prefix 😀 suffix", "prefix 🙂 suffix"],
  ["line1\nline2\nline3", "line1\nline2 edited\nline3"],
];

test("diffText round-trips through applyDiff", () => {
  for (const [a, b] of cases) {
    const d = diffText(a, b);
    const result = d ? applyDiff(a, d) : a;
    assert.equal(result, b, `${JSON.stringify(a)} -> ${JSON.stringify(b)}`);
  }
});

test("diffText applied to Y.Text converges", () => {
  for (const [a, b] of cases) {
    const doc = new Y.Doc();
    const t = doc.getText("t");
    t.insert(0, a);
    const d = diffText(t.toString(), b);
    if (d) {
      doc.transact(() => {
        if (d.remove) t.delete(d.index, d.remove);
        if (d.insert) t.insert(d.index, d.insert);
      });
    }
    assert.equal(t.toString(), b);
  }
});

test("concurrent edits on two docs merge without loss", () => {
  const d1 = new Y.Doc();
  const d2 = new Y.Doc();
  const t1 = d1.getText("t");
  const t2 = d2.getText("t");
  t1.insert(0, "The quick brown fox");
  Y.applyUpdate(d2, Y.encodeStateAsUpdate(d1));

  const e1 = diffText(t1.toString(), "The very quick brown fox")!;
  d1.transact(() => {
    if (e1.remove) t1.delete(e1.index, e1.remove);
    t1.insert(e1.index, e1.insert);
  });
  const e2 = diffText(t2.toString(), "The quick brown fox jumps")!;
  d2.transact(() => {
    if (e2.remove) t2.delete(e2.index, e2.remove);
    t2.insert(e2.index, e2.insert);
  });

  Y.applyUpdate(d1, Y.encodeStateAsUpdate(d2));
  Y.applyUpdate(d2, Y.encodeStateAsUpdate(d1));
  assert.equal(t1.toString(), "The very quick brown fox jumps");
  assert.equal(t2.toString(), t1.toString());
});

test("hashText is stable and distinguishes content", () => {
  assert.equal(hashText("abc"), hashText("abc"));
  assert.notEqual(hashText("abc"), hashText("abd"));
  assert.equal(hashText("").length, 16);
});
