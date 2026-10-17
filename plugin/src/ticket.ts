/**
 * Share tickets, decoded locally so the plugin can read one without any server:
 * `samsyn` + lowercase base32 (no padding) of a postcard-encoded struct.
 * Mirrors server/src/ticket.rs; v1 has no url, v2 appends it, v3 appends rwPaths.
 */
export interface Ticket {
  version: number;
  shareId: string;
  name: string;
  nodeId: string;
  secret: string;
  access: string;
  addrs: string[];
  /** Owner server's public URL (v2). Empty = guests cannot use this ticket. */
  url: string;
  /** Share-relative paths a read-only ticket may still write (v3); folders end with "/". */
  rwPaths: string[];
}

const PREFIX = "samsyn";
const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

function base32Decode(s: string): Uint8Array {
  const out: number[] = [];
  let bits = 0;
  let value = 0;
  for (const ch of s.toUpperCase()) {
    const v = ALPHABET.indexOf(ch);
    if (v < 0) throw new Error("ticket is not valid base32");
    value = (value << 5) | v;
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return Uint8Array.from(out);
}

function base32Encode(bytes: Uint8Array): string {
  let out = "";
  let bits = 0;
  let value = 0;
  for (const b of bytes) {
    value = (value << 8) | b;
    bits += 8;
    while (bits >= 5) {
      out += ALPHABET[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
  }
  if (bits > 0) out += ALPHABET[(value << (5 - bits)) & 31];
  return out.toLowerCase();
}

class Reader {
  pos = 0;
  constructor(private buf: Uint8Array) {}
  u8(): number {
    if (this.pos >= this.buf.length) throw new Error("ticket payload is truncated");
    return this.buf[this.pos++];
  }
  varint(): number {
    let n = 0;
    let shift = 0;
    for (;;) {
      const b = this.u8();
      n += (b & 0x7f) * 2 ** shift;
      if ((b & 0x80) === 0) return n;
      shift += 7;
      if (shift > 35) throw new Error("ticket payload is malformed");
    }
  }
  string(): string {
    const len = this.varint();
    if (this.pos + len > this.buf.length) throw new Error("ticket payload is truncated");
    const s = new TextDecoder().decode(this.buf.subarray(this.pos, this.pos + len));
    this.pos += len;
    return s;
  }
}

class Writer {
  private out: number[] = [];
  u8(b: number): void {
    this.out.push(b & 0xff);
  }
  varint(n: number): void {
    while (n >= 0x80) {
      this.out.push((n & 0x7f) | 0x80);
      n = Math.floor(n / 128);
    }
    this.out.push(n);
  }
  string(s: string): void {
    const bytes = new TextEncoder().encode(s);
    this.varint(bytes.length);
    for (const b of bytes) this.out.push(b);
  }
  bytes(): Uint8Array {
    return Uint8Array.from(this.out);
  }
}

export function decodeTicket(raw: string): Ticket {
  const s = raw.trim();
  if (!s.toLowerCase().startsWith(PREFIX)) throw new Error("not a samsyn ticket");
  const r = new Reader(base32Decode(s.slice(PREFIX.length)));
  const version = r.u8();
  if (version !== 1 && version !== 2 && version !== 3) throw new Error(`unsupported ticket version ${version}; update the plugin`);
  const shareId = r.string();
  const name = r.string();
  const nodeId = r.string();
  const secret = r.string();
  const access = r.string();
  const n = r.varint();
  const addrs: string[] = [];
  for (let i = 0; i < n; i++) addrs.push(r.string());
  const url = version >= 2 ? r.string() : "";
  const rwPaths: string[] = [];
  if (version >= 3) {
    const m = r.varint();
    for (let i = 0; i < m; i++) rwPaths.push(r.string());
  }
  return { version, shareId, name, nodeId, secret, access, addrs, url, rwPaths };
}

/** Encoder, used by tests to round-trip; the server is the only real issuer. */
export function encodeTicket(t: Ticket): string {
  const w = new Writer();
  w.u8(t.version);
  w.string(t.shareId);
  w.string(t.name);
  w.string(t.nodeId);
  w.string(t.secret);
  w.string(t.access);
  w.varint(t.addrs.length);
  for (const a of t.addrs) w.string(a);
  if (t.version >= 2) w.string(t.url);
  if (t.version >= 3) {
    w.varint(t.rwPaths.length);
    for (const p of t.rwPaths) w.string(p);
  }
  return PREFIX + base32Encode(w.bytes());
}
