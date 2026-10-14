import type { DataAdapter } from "obsidian";
import * as Y from "yjs";

/** Local on-disk cache of the Yjs doc and the per-file "last synced" hashes. */
export interface LocalState {
  /** path -> content hash (text: hashText, blob: sha256) at the moment it was last in sync */
  synced: Record<string, string>;
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

  async loadState(): Promise<LocalState> {
    try {
      if (await this.adapter.exists(this.statePath)) {
        const parsed = JSON.parse(await this.adapter.read(this.statePath)) as Partial<LocalState>;
        return { synced: parsed.synced ?? {} };
      }
    } catch (e) {
      console.warn("[replica] failed to load sync state", e);
    }
    return { synced: {} };
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
