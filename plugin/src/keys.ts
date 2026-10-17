/** Vault-relative, "/"-separated, no empty / "." / ".." segments, no control chars. */
export function isSafeKey(key: string): boolean {
  if (!key || key.length > 1024 || key.startsWith("/") || key.endsWith("/") || key.includes("\\")) return false;
  for (let i = 0; i < key.length; i++) {
    const c = key.charCodeAt(i);
    if (c < 0x20 || c === 0x7f) return false;
  }
  return key.split("/").every((seg) => seg !== "" && seg !== "." && seg !== "..");
}

