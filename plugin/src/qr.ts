import qrcode from "qrcode-generator";

/** Deep links handled by the plugin (`registerObsidianProtocolHandler("samsyn", …)`). */
export const PROTOCOL = "obsidian://samsyn";

export function mountLink(ticket: string): string {
  return `${PROTOCOL}?op=mount&ticket=${encodeURIComponent(ticket.trim())}`;
}

/**
 * Note: `vault` is reserved in obsidian:// links (Obsidian uses it to pick a local vault and
 * drops the link when none matches), so the server-side vault id travels as `vid`.
 */
export function setupLink(server: string, token: string, vaultId: string, user?: string): string {
  const q = new URLSearchParams({ op: "setup", server, token, vid: vaultId });
  if (user) q.set("user", user);
  return `${PROTOCOL}?${q.toString()}`;
}

/** QR code as an inline SVG string (scalable; the caller sizes it with CSS). */
export function qrSvg(text: string): string {
  const qr = qrcode(0, "M");
  qr.addData(text);
  qr.make();
  return qr.createSvgTag({ cellSize: 4, margin: 2, scalable: true });
}

/** Render a QR for `text` into `el` with a caption; falls back to text when it is too long. */
export function renderQr(el: HTMLElement, text: string, caption: string): void {
  el.empty();
  el.addClass("samsyn-qr");
  try {
    const box = el.createDiv({ cls: "samsyn-qr-image" });
    // The SVG comes from a local library with our own text; no remote content involved.
    box.innerHTML = qrSvg(text);
  } catch (e) {
    el.createDiv({ cls: "samsyn-error", text: `Too much data for a QR code (${(e as Error).message}); use the link instead.` });
  }
  el.createDiv({ cls: "samsyn-muted samsyn-small", text: caption });
}
