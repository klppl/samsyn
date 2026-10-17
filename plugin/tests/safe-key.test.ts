import { test } from "node:test";
import assert from "node:assert/strict";
import { isSafeKey } from "../src/keys";

test("safe keys", () => {
  for (const k of ["a.md", "Projects/X/a.md", "deep/er/path/with space.png"]) assert.equal(isSafeKey(k), true, k);
});

test("unsafe keys never map to files", () => {
  for (const k of ["", "/etc/passwd", "../x.md", "a/../../x.md", "a/./b", "a//b", "a/", "a\\b", "a\u0000b", "a\nb", "a".repeat(1025)]) {
    assert.equal(isSafeKey(k), false, JSON.stringify(k));
  }
});
