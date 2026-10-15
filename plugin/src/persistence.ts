import type { DataAdapter } from "obsidian";
import * as Y from "yjs";
import { IndexeddbPersistence } from "y-indexeddb";
import * as idb from "lib0/indexeddb";

const STATE_STORE = "synced";

/** Local cache of the Yjs doc and the per-file "last synced" hashes. */
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

export function hasIndexedDb(): boolean {
  try {
    return typeof indexedDB !== "undefined" && indexedDB !== null;
  } catch {
    return false;
  }
}

export interface StorageInfo {
  backend: "indexeddb" | "files";
  /** Encoded size of the whole Yjs document (what a full save would write). */
  docBytes: number;
  /** Bytes used by this origin's IndexedDB according to the browser, if known. */
  usageBytes: number | null;
  quotaBytes: number | null;
  syncedEntries: number;
}

/**
 * Storage for the local cache.
 *
 * Preferred backend is IndexedDB (desktop Electron and the mobile WebViews all have it): the
 * document is persisted by `y-indexeddb` as an append-only log of updates that is compacted
 * every 500 entries, so a keystroke costs one small record instead of rewriting the whole
 * document; the synced-hash map is one record per path. Both live outside the vault folder,
 * so a copied or cloud-synced `.obsidian` can never carry another device's bookkeeping.
 *
 * Fallback (no IndexedDB): the previous `doc.bin` + `state.json` files in the plugin folder.
 * Existing files are migrated into IndexedDB on first use and then removed.
 */
export class Persistence {
  readonly backend: "indexeddb" | "files";
  private idb: IndexeddbPersistence | null = null;
  private stateDb: IDBDatabase | null = null;
  private doc: Y.Doc | null = null;

  private pendingSet = new Map<string, string>();
  private pendingDel = new Set<string>();
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private flushing: Promise<void> = Promise.resolve();

  // files fallback
  private docTimer: ReturnType<typeof setTimeout> | null = null;
  private stateTimer: ReturnType<typeof setTimeout> | null = null;
  private lastState: LocalState | null = null;

  private docBytesCache: { at: number; bytes: number } | null = null;

  /**
   * @param scope identifies this install + vault; IndexedDB is shared per origin, and on desktop
   *   every vault runs on the same origin, so the database names must include it.
   */
  constructor(
    private adapter: DataAdapter,
    private dir: string,
    private scope: string,
    forceFiles = false,
  ) {
    this.backend = !forceFiles && hasIndexedDb() ? "indexeddb" : "files";
  }

  private get docPath() {
    return `${this.dir}/doc.bin`;
  }
  private get statePath() {
    return `${this.dir}/state.json`;
  }
  private dbName(kind: "doc" | "state"): string {
    return `replica-${kind}-${this.scope.replace(/[^A-Za-z0-9_.:-]/g, "_")}`;
  }

  // ------------------------------------------------------------------ document

  /**
   * Bind the document to its store and load what is there. Returns true when a cached
   * document existed (so the engine knows this is not the first run on this device).
   */
  async loadDoc(doc: Y.Doc): Promise<boolean> {
    this.doc = doc;
    if (this.backend === "files") {
      const had = await this.loadDocFromFile(doc);
      doc.on("update", () => this.scheduleSaveDoc(doc));
      return had;
    }
    this.idb = new IndexeddbPersistence(this.dbName("doc"), doc);
    await this.idb.whenSynced;
    let had = docHasContent(doc);
    if (!had && (await this.adapter.exists(this.docPath))) {
      // One-time migration from the file cache; y-indexeddb persists the applied update.
      had = await this.loadDocFromFile(doc);
      if (had) console.info("[replica] migrated doc.bin into IndexedDB");
      await this.adapter.remove(this.docPath).catch(() => undefined);
    }
    return had;
  }

  private async loadDocFromFile(doc: Y.Doc): Promise<boolean> {
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

  private scheduleSaveDoc(doc: Y.Doc, delayMs = 1500): void {
    if (this.docTimer) clearTimeout(this.docTimer);
    this.docTimer = setTimeout(() => void this.saveDocToFile(doc), delayMs);
  }

  private async saveDocToFile(doc: Y.Doc): Promise<void> {
    if (this.docTimer) {
      clearTimeout(this.docTimer);
      this.docTimer = null;
    }
    const update = Y.encodeStateAsUpdate(doc);
    const buf = update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength) as ArrayBuffer;
    await this.adapter.writeBinary(this.docPath, buf);
  }

  // ------------------------------------------------------------------ synced hashes

  async loadState(installId: string): Promise<LocalState> {
    if (this.backend === "indexeddb") {
      let synced: Record<string, string> = {};
      try {
        this.stateDb = await idb.openDB(this.dbName("state"), (db) => idb.createStores(db, [[STATE_STORE]]));
        const [store] = idb.transact(this.stateDb, [STATE_STORE], "readonly");
        for (const { k, v } of await idb.getAllKeysValues(store)) synced[String(k)] = String(v);
      } catch (e) {
        console.warn("[replica] failed to read sync state from IndexedDB", e);
      }
      if (Object.keys(synced).length === 0 && (await this.adapter.exists(this.statePath))) {
        const legacy = await this.loadStateFromFile(installId);
        synced = legacy.synced;
        if (Object.keys(synced).length > 0) {
          await this.writeEntries(Object.entries(synced), []).catch((e) =>
            console.warn("[replica] migrating sync state failed", e),
          );
          console.info(`[replica] migrated ${Object.keys(synced).length} sync entries into IndexedDB`);
        }
        await this.adapter.remove(this.statePath).catch(() => undefined);
      }
      return { installId, synced };
    }
    const st = await this.loadStateFromFile(installId);
    this.lastState = st;
    return st;
  }

  private async loadStateFromFile(installId: string): Promise<LocalState> {
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

  /** Record that `path` is in sync at `hash`. Batched; durable within about a second. */
  noteSynced(path: string, hash: string, state: LocalState): void {
    this.pendingDel.delete(path);
    this.pendingSet.set(path, hash);
    this.scheduleFlush(state);
  }

  noteForgotten(path: string, state: LocalState): void {
    this.pendingSet.delete(path);
    this.pendingDel.add(path);
    this.scheduleFlush(state);
  }

  private scheduleFlush(state: LocalState, delayMs = 1000): void {
    this.lastState = state;
    if (this.flushTimer) clearTimeout(this.flushTimer);
    this.flushTimer = setTimeout(() => void this.flushState(state), delayMs);
  }

  /** Write every pending synced-hash change now. */
  async flushState(state: LocalState): Promise<void> {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.lastState = state;
    // Serialize flushes so a slow IndexedDB transaction cannot interleave with the next one.
    this.flushing = this.flushing.then(() => this.flushNow(state)).catch((e) => {
      console.warn("[replica] saving sync state failed", e);
    });
    await this.flushing;
  }

  private async flushNow(state: LocalState): Promise<void> {
    if (this.backend === "files") {
      this.pendingSet.clear();
      this.pendingDel.clear();
      if (this.stateTimer) {
        clearTimeout(this.stateTimer);
        this.stateTimer = null;
      }
      await this.adapter.write(this.statePath, JSON.stringify(state));
      return;
    }
    const sets = [...this.pendingSet.entries()];
    const dels = [...this.pendingDel];
    this.pendingSet.clear();
    this.pendingDel.clear();
    if (sets.length || dels.length) await this.writeEntries(sets, dels);
  }

  private async writeEntries(sets: [string, string][], dels: string[]): Promise<void> {
    if (!this.stateDb) return;
    const [store] = idb.transact(this.stateDb, [STATE_STORE]);
    await Promise.all([...sets.map(([k, v]) => idb.put(store, v, k)), ...dels.map((k) => idb.del(store, k))]);
  }

  // ------------------------------------------------------------------ lifecycle

  /** Flush and release the stores. Call before `doc.destroy()`. */
  async close(): Promise<void> {
    if (this.lastState) await this.flushState(this.lastState);
    if (this.backend === "files" && this.doc) await this.saveDocToFile(this.doc);
    if (this.idb) {
      await this.idb.destroy();
      this.idb = null;
    }
    if (this.stateDb) {
      this.stateDb.close();
      this.stateDb = null;
    }
    this.doc = null;
  }

  /** Remove every cached byte for this install + vault (both backends, both generations). */
  async clear(): Promise<void> {
    await this.close();
    if (hasIndexedDb()) {
      // Our own connections are closed above; any other open connection (another vault window)
      // closes itself on versionchange. Do not wait forever if one does not.
      for (const kind of ["doc", "state"] as const) {
        await Promise.race([
          idb.deleteDB(this.dbName(kind)).catch((e) => console.warn("[replica] deleting cache db failed", e)),
          new Promise<void>((resolve) => setTimeout(resolve, 5000)),
        ]);
      }
    }
    for (const p of [this.docPath, this.statePath]) {
      if (await this.adapter.exists(p)) await this.adapter.remove(p);
    }
  }

  /** Sizes for the status section. The document size is recomputed at most once a minute. */
  async storageInfo(doc: Y.Doc, syncedEntries: number): Promise<StorageInfo> {
    const now = Date.now();
    if (!this.docBytesCache || now - this.docBytesCache.at > 60_000) {
      this.docBytesCache = { at: now, bytes: Y.encodeStateAsUpdate(doc).byteLength };
    }
    let usageBytes: number | null = null;
    let quotaBytes: number | null = null;
    try {
      const est = await navigator.storage?.estimate?.();
      if (est) {
        usageBytes = est.usage ?? null;
        quotaBytes = est.quota ?? null;
      }
    } catch {
      /* not available */
    }
    return { backend: this.backend, docBytes: this.docBytesCache.bytes, usageBytes, quotaBytes, syncedEntries };
  }
}

function docHasContent(doc: Y.Doc): boolean {
  return doc.getMap("texts").size > 0 || doc.getMap("blobs").size > 0 || doc.store.clients.size > 0;
}
