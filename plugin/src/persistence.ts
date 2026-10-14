import type { DataAdapter } from "obsidian";
import * as Y from "yjs";

/** Local on-disk cache of the Yjs doc and the per-file "last synced" hashes. */
export interface LocalState {
  /** Identifies this plugin install on this device. If it does not match, `synced` is discarded. */
  installId?: string;
  /** path -> content hash (text: hashText, blob: sha256) at the moment it was last in sync */
  synced: Record<string, string>;
}

/**
 * A per-device id that does NOT live in the vault folder, so copying `.obsidian` to another
 * device cannot carry over "I have seen these files" bookkeeping. Prefer Obsidian's own
 * per-install appId, fall back to localStorage, then to a fresh id per session.
 */
export function deviceInstallId(app: unknown): string {
  const appId = (app as { appId?: unknown }).appId;
  if (typeof appId === "string" && appId.length > 0) return "app:" + appId;
  try {
    const key = "replica-install-id";
    let v = globalThis.localStorage?.getItem(key);
    if (!v) {
      v = "ls:" + Math.random().toString(36).slice(2) + Date.now().toString(36);
      globalThis.localStorage?.setItem(key, v);
    }
    return v;
  } catch {
    return "tmp:" + Math.random().toString(36).slice(2);
  }
}

export class Persistence {
  private docTimer: ReturnType<typeof setTimeout> | null = null;
  private stateTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private adapter: DataAdapter, private dir: string) {}

  private get docPath() {
    return `${this.dir}/doc.bin`;
  }
  private get statePath() {
    return `${this.dir}/state.json`;
  }

  async hasDoc(): Promise<boolean> {
    return this.adapter.exists(this.docPath);
  }

  async loadDoc(doc: Y.Doc): Promise<boolean> {
    if (!(await this.adapter.exists(this.docPath))) return false;
    try {
      const buf = await this.adapter.readBinary(this.docPath);
      Y.applyUpdate(doc, new Uint8Array(buf), "persistence");
      return true;
    } catch (e) {
      console.warn("[replica] failed to load cached doc, starting fresh", e);
      return false;
    }
  }

  scheduleSaveDoc(doc: Y.Doc, delayMs = 1500): void {
    if (this.docTimer) clearTimeout(this.docTimer);
    this.docTimer = setTimeout(() => void this.saveDoc(doc), delayMs);
  }

  async saveDoc(doc: Y.Doc): Promise<void> {
    if (this.docTimer) {
      clearTimeout(this.docTimer);
      this.docTimer = null;
    }
    const update = Y.encodeStateAsUpdate(doc);
    const buf = update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength) as ArrayBuffer;
    await this.adapter.writeBinary(this.docPath, buf);
  }

  async loadState(installId: string): Promise<LocalState> {
    try {
      if (await this.adapter.exists(this.statePath)) {
        const parsed = JSON.parse(await this.adapter.read(this.statePath)) as Partial<LocalState>;
        if (parsed.installId !== installId) {
          console.warn(
            `[replica] sync state belongs to another install (${parsed.installId ?? "none"} vs ${installId}); starting fresh`,
          );
          return { installId, synced: {} };
        }
        return { installId, synced: parsed.synced ?? {} };
      }
    } catch (e) {
      console.warn("[replica] failed to load sync state", e);
    }
    return { installId, synced: {} };
  }

  scheduleSaveState(state: LocalState, delayMs = 1000): void {
    if (this.stateTimer) clearTimeout(this.stateTimer);
    this.stateTimer = setTimeout(() => void this.saveState(state), delayMs);
  }

  async saveState(state: LocalState): Promise<void> {
    if (this.stateTimer) {
      clearTimeout(this.stateTimer);
      this.stateTimer = null;
    }
    await this.adapter.write(this.statePath, JSON.stringify(state));
  }

  async clear(): Promise<void> {
    for (const p of [this.docPath, this.statePath]) {
      if (await this.adapter.exists(p)) await this.adapter.remove(p);
    }
  }
}
