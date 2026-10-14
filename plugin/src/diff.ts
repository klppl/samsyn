/**
 * Minimal single-span text diff: common prefix + common suffix.
 * Good enough for live typing (one contiguous edit per debounce window),
 * and the CRDT handles concurrency regardless of how coarse the diff is.
 */
export interface TextDiff {
  index: number;
  remove: number;
  insert: string;
}

export function diffText(a: string, b: string): TextDiff | null {
  if (a === b) return null;
  const maxStart = Math.min(a.length, b.length);
  let start = 0;
  while (start < maxStart && a.charCodeAt(start) === b.charCodeAt(start)) start++;
  // Never split a UTF-16 surrogate pair.
  if (start > 0 && isLowSurrogate(a.charCodeAt(start)) && isHighSurrogate(a.charCodeAt(start - 1))) start--;

  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a.charCodeAt(endA - 1) === b.charCodeAt(endB - 1)) {
    endA--;
    endB--;
  }
  if (endA < a.length && isHighSurrogate(a.charCodeAt(endA - 1)) && isLowSurrogate(a.charCodeAt(endA))) {
    endA++;
    endB++;
  }
  return { index: start, remove: endA - start, insert: b.slice(start, endB) };
}

function isHighSurrogate(c: number): boolean {
  return c >= 0xd800 && c <= 0xdbff;
}
function isLowSurrogate(c: number): boolean {
  return c >= 0xdc00 && c <= 0xdfff;
}

export function applyDiff(a: string, d: TextDiff): string {
  return a.slice(0, d.index) + d.insert + a.slice(d.index + d.remove);
}
