import { test } from "node:test";
import assert from "node:assert/strict";
import { decodeTicket, encodeTicket } from "../src/ticket";

// Issued by replica-server 0.1 (ticket v2) for share fc6634d6ac5d34e6 on a server started
// with --public-url https://sync.example.test.
const SERVER_TICKET =
  "replicaaiigmyzwgyztizbwmfrtkzbtgrstmbcqnrqw4qbzmfsdqyrsme2tqnjsmvtgiolbgi3dgzbwmu2dgmjwg4ytanzqgi4tiyrtgzrwkmbxmzswmojqgbsdczrxmq2dondcgu4tczlfmizgcqbsgzsdgzjvgjqtkyjtmqzgcobqmi4donlcmyydmndeheztkzlcmjrtmzrymyydentegy4demzsgnsdmm3fmzsdoodggi4tgnzrmzstaatso4abs2duoryhgorpf5zxs3tdfzsxqylnobwgkltumvzxi";

test("decodes a ticket issued by the server", () => {
  const t = decodeTicket(SERVER_TICKET);
  assert.equal(t.version, 2);
  assert.equal(t.shareId, "fc6634d6ac5d34e6");
  assert.equal(t.name, "Plan");
  assert.equal(t.nodeId, "9ad8b2a5852efd9a263d6e431671070294b36ce07fef900d1f7d474b591eeb2a");
  assert.equal(t.secret.length, 64);
  assert.equal(t.access, "rw");
  assert.deepEqual(t.addrs, []);
  assert.equal(t.url, "https://sync.example.test");
  // whitespace and case do not matter
  assert.deepEqual(decodeTicket("  " + SERVER_TICKET.toUpperCase().replace("REPLICA", "replica") + "\n"), t);
});

test("round-trips v1 and v2", () => {
  const t = {
    version: 2,
    shareId: "0123456789abcdef",
    name: "Project X",
    nodeId: "ab".repeat(32),
    secret: "cd".repeat(32),
    access: "ro",
    addrs: ["relay:https://r.example/", "ip:1.2.3.4:5"],
    url: "https://sync.example",
  };
  assert.deepEqual(decodeTicket(encodeTicket(t)), t);
  const v1 = { ...t, version: 1, url: "" };
  assert.deepEqual(decodeTicket(encodeTicket(v1)), v1);
});

test("rejects garbage", () => {
  assert.throws(() => decodeTicket("hello"), /not a replica ticket/);
  assert.throws(() => decodeTicket("replica!!!"), /base32/);
  assert.throws(() => decodeTicket("replicaxxxx"), /truncated|malformed|version/);
});
