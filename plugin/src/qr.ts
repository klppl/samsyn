import qrcode from "qrcode-generator";

/** Deep links handled by the plugin (`registerObsidianProtocolHandler("replica", …)`). */
export const PROTOCOL = "obsidian://replica";

export function mountLink(ticket: string): string {
  return `${PROTOCOL}?op=mount&ticket=${encodeURIComponent(ticket.trim())}`;
}

export function setupLink(server: string, token: string, vault: string, user?: string): string {
  const q = new URLSearchParams({ op: "setup", server, token, vault });
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
  el.addClass("replica-qr");
  try {
    const box = el.createDiv({ cls: "replica-qr-image" });
    // The SVG comes from a local library with our own text; no remote content involved.
    box.innerHTML = qrSvg(text);
  } catch (e) {
    el.createDiv({ cls: "replica-error", text: `Too much data for a QR code (${(e as Error).message}); use the link instead.` });
  }
  el.createDiv({ cls: "replica-muted replica-small", text: caption });
}
