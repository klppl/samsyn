import { App, Editor, MarkdownView, Notice, Platform, TAbstractFile, TFile, TFolder, normalizePath } from "obsidian";
import * as Y from "yjs";
import { WebsocketProvider } from "y-websocket";
import { BlobClient } from "./blobs";
import { diffText } from "./diff";
import { hashText, sha256Hex } from "./hash";
import { LocalState, Persistence } from "./persistence";
import { ReplicaSettings, serverUrls } from "./settings";

export interface BlobMeta {
  hash: string;
  size: number;
  mtime: number;
}

export type EngineStatus = "disabled" | "offline" | "connecting" | "connected";

export interface EngineEvents {
  onStatus(status: EngineStatus, peers: number): void;
}

export interface Diagnostics {
  status: EngineStatus;
  wsUrl: string;
  synced: boolean;
  peers: number;
  textFiles: number;
  blobFiles: number;
  lastError: string | null;
  lastConnectedAt: number | null;
  hint: string | null;
}

/**
 * Document layout (one Y.Doc per vault):
 *   texts: Y.Map<path, Y.Text>   character-level merged files (.md, .txt, ...)
 *   blobs: Y.Map<path, BlobMeta> last-writer-wins pointers to content-addressed blobs
 */
export class SyncEngine {
  readonly doc = new Y.Doc({ gc: true });
  readonly texts: Y.Map<Y.Text>;
  readonly blobs: Y.Map<BlobMeta>;
  /** Transactions we originate carry this origin so observers can ignore them. */
  readonly localOrigin = { replica: "local" };

  private provider: WebsocketProvider | null = null;
  private blobClient: BlobClient | null = null;
  private state: LocalState = { synced: {} };
  private editorTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private textExts: Set<string>;
  private excludes: string[];
  private stopped = false;
  private unsubscribers: Array<() => void> = [];
  private status: EngineStatus = "offline";
  private wsUrl = "";
  private lastError: string | null = null;
  private lastConnectedAt: number | null = null;
  private hint: string | null = null;

  constructor(
    private app: App,
    private settings: ReplicaSettings,
    private persistence: Persistence,
    private events: EngineEvents,
  ) {
    this.texts = this.doc.getMap<Y.Text>("texts");
    this.blobs = this.doc.getMap<BlobMeta>("blobs");
    this.textExts = new Set(
      settings.textExtensions
        .split(",")
        .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
        .filter(Boolean),
    );
    this.excludes = settings.excludePaths
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
  }

  // ---------------------------------------------------------------- lifecycle

  async start(): Promise<void> {
    this.state = await this.persistence.loadState();
    const hadDoc = await this.persistence.loadDoc(this.doc);

    this.doc.on("update", () => this.persistence.scheduleSaveDoc(this.doc));

    const urls = serverUrls(this.settings.serverUrl);
    if (urls && this.settings.vaultId) {
      this.blobClient = new BlobClient(urls.http, this.settings.vaultId, this.settings.token);
      this.connect(urls.ws);
      if (!hadDoc) {
        // First run on this device: wait for the server state before reconciling so that
        // files which already exist on both sides are merged, not duplicated.
        await this.waitForSync(10_000);
      }
    } else {
      this.setStatus("disabled");
    }

    await this.reconcileAll();
    this.installObservers();
    this.installVaultListeners();
  }

  async stop(): Promise<void> {
    this.stopped = true;
    for (const t of this.editorTimers.values()) clearTimeout(t);
    this.editorTimers.clear();
    for (const u of this.unsubscribers) u();
    this.unsubscribers = [];
    this.provider?.destroy();
    this.provider = null;
    await this.persistence.saveDoc(this.doc);
    await this.persistence.saveState(this.state);
    this.doc.destroy();
  }

  get isConnected(): boolean {
    return this.status === "connected";
  }

  private connect(wsBase: string): void {
    this.setStatus("connecting");
    this.wsUrl = `${wsBase}/${this.settings.vaultId}`;
    if (wsBase.startsWith("ws://") && Platform.isMobileApp) {
      this.hint =
        "Plain ws:// is blocked on mobile (insecure WebSocket from a secure app). Use an https:// server URL with a valid certificate.";
    } else if (typeof WebSocket === "undefined") {
      this.hint = "WebSocket is not available in this environment.";
    } else {
      this.hint = null;
    }
    let provider: WebsocketProvider;
    try {
      provider = new WebsocketProvider(wsBase, this.settings.vaultId, this.doc, {
        params: { token: this.settings.token },
        disableBc: true,
        maxBackoffTime: 10_000,
        resyncInterval: 60_000,
      });
    } catch (e) {
      this.lastError = `WebSocket could not be created: ${(e as Error).message}`;
      console.error("[replica]", this.lastError);
      this.setStatus("offline");
      return;
    }
    provider.awareness.setLocalStateField("user", { name: this.settings.deviceName });
    provider.on("status", ({ status }: { status: string }) => {
      if (status === "connected") {
        this.lastError = null;
        this.lastConnectedAt = Date.now();
        this.setStatus("connected");
      } else if (status === "connecting") this.setStatus("connecting");
      else this.setStatus("offline");
    });
    provider.on("connection-error", (ev: Event) => {
      this.lastError = `WebSocket error (${ev?.type ?? "error"}) connecting to ${this.wsUrl}`;
      console.warn("[replica]", this.lastError, ev);
      this.setStatus("offline");
    });
    provider.on("connection-close", (ev: CloseEvent | null) => {
      if (ev && ev.code !== 1000 && ev.code !== 1005) {
        this.lastError = `WebSocket closed: code ${ev.code}${ev.reason ? " " + ev.reason : ""}`;
        if (ev.code === 1006) this.lastError += " (server unreachable, TLS/certificate problem, or blocked insecure ws://)";
      }
    });
    provider.awareness.on("change", () => this.setStatus(this.status));
    this.provider = provider;
  }

  getDiagnostics(): Diagnostics {
    return {
      status: this.status,
      wsUrl: this.wsUrl,
      synced: this.provider?.synced ?? false,
      peers: this.provider ? Math.max(0, this.provider.awareness.getStates().size - 1) : 0,
      textFiles: this.texts.size,
      blobFiles: this.blobs.size,
      lastError: this.lastError,
      lastConnectedAt: this.lastConnectedAt,
      hint: this.hint,
    };
  }

  private waitForSync(timeoutMs: number): Promise<void> {
    const p = this.provider;
    if (!p) return Promise.resolve();
    return new Promise((resolve) => {
      if (p.synced) return resolve();
      const timer = setTimeout(() => {
        p.off("sync", onSync);
        resolve();
      }, timeoutMs);
      const onSync = (synced: boolean) => {
        if (synced) {
          clearTimeout(timer);
          p.off("sync", onSync);
          resolve();
        }
      };
      p.on("sync", onSync);
    });
  }

  private setStatus(s: EngineStatus): void {
    this.status = s;
    const peers = this.provider ? Math.max(0, this.provider.awareness.getStates().size - 1) : 0;
    this.events.onStatus(s, peers);
  }

  setActiveFile(path: string | null): void {
    this.provider?.awareness.setLocalStateField("file", path);
  }

  // ---------------------------------------------------------------- helpers

  private isExcluded(path: string): boolean {
    if (path.startsWith(this.app.vault.configDir + "/") || path.startsWith(".")) return true;
    return this.excludes.some((e) => path === e || path.startsWith(e.endsWith("/") ? e : e + "/") || path.startsWith(e));
  }

  private isText(path: string): boolean {
    const dot = path.lastIndexOf(".");
    if (dot < 0) return false;
    return this.textExts.has(path.slice(dot + 1).toLowerCase());
  }

  private shouldSync(path: string): boolean {
    if (this.isExcluded(path)) return false;
    if (this.isText(path)) return true;
    return this.settings.syncBinary;
  }

  private markSynced(path: string, hash: string): void {
    this.state.synced[path] = hash;
    this.persistence.scheduleSaveState(this.state);
  }

  private forgetSynced(path: string): void {
    if (path in this.state.synced) {
      delete this.state.synced[path];
      this.persistence.scheduleSaveState(this.state);
    }
  }

  private async ensureFolder(path: string): Promise<void> {
    const idx = path.lastIndexOf("/");
    if (idx <= 0) return;
    const folder = path.slice(0, idx);
    if (this.app.vault.getAbstractFileByPath(folder)) return;
    await this.ensureFolder(folder);
    try {
      await this.app.vault.createFolder(folder);
    } catch {
      /* raced with another create */
    }
  }

  private conflictPath(path: string): string {
    const dot = path.lastIndexOf(".");
    const slash = path.lastIndexOf("/");
    const base = dot > slash ? path.slice(0, dot) : path;
    const ext = dot > slash ? path.slice(dot) : "";
    const d = new Date();
    const stamp = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}${pad(d.getMinutes())}`;
    return `${base} (conflict ${this.settings.deviceName} ${stamp})${ext}`;
  }

  // ---------------------------------------------------------------- disk writes

  private async writeTextToDisk(path: string, content: string): Promise<void> {
    this.markSynced(path, hashText(content));
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      await this.app.vault.modify(existing, content);
    } else {
      await this.ensureFolder(path);
      await this.app.vault.create(path, content);
    }
  }

  private async writeBinaryToDisk(path: string, data: ArrayBuffer, hash: string): Promise<void> {
    this.markSynced(path, hash);
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      await this.app.vault.modifyBinary(existing, data);
    } else {
      await this.ensureFolder(path);
      await this.app.vault.createBinary(path, data);
    }
  }

  private async deleteFromDisk(path: string): Promise<void> {
    this.forgetSynced(path);
    const f = this.app.vault.getAbstractFileByPath(path);
    if (f instanceof TFile) {
      await this.app.fileManager.trashFile(f);
    }
  }

  private async writeConflictCopy(path: string, content: string | ArrayBuffer): Promise<void> {
    const cp = this.conflictPath(path);
    await this.ensureFolder(cp);
    if (typeof content === "string") await this.app.vault.create(cp, content);
    else await this.app.vault.createBinary(cp, content);
    new Notice(`Replica: conflict on ${path}, kept your version as ${cp.split("/").pop()}`);
  }

  /** Editors currently showing a file, so remote changes can be applied without losing the cursor. */
  private editorsFor(path: string): Editor[] {
    const out: Editor[] = [];
    for (const leaf of this.app.workspace.getLeavesOfType("markdown")) {
      const view = leaf.view;
      if (view instanceof MarkdownView && view.file?.path === path && view.editor) out.push(view.editor);
    }
    return out;
  }

  // ---------------------------------------------------------------- local -> doc

  /** Push the current local content of a text file into the shared doc. */
  pushLocalText(path: string, content: string): void {
    if (this.stopped || !this.shouldSync(path) || !this.isText(path)) return;
    const existing = this.texts.get(path);
    this.doc.transact(() => {
      if (!existing) {
        this.texts.set(path, new Y.Text(content));
        return;
      }
      const d = diffText(existing.toString(), content);
      if (!d) return;
      if (d.remove > 0) existing.delete(d.index, d.remove);
      if (d.insert) existing.insert(d.index, d.insert);
    }, this.localOrigin);
    this.markSynced(path, hashText(content));
  }

  private async pushLocalBlob(path: string, file: TFile): Promise<void> {
    if (this.stopped || !this.shouldSync(path) || this.isText(path)) return;
    const data = await this.app.vault.readBinary(file);
    const hash = await sha256Hex(data);
    const current = this.blobs.get(path);
    if (current?.hash === hash) {
      this.markSynced(path, hash);
      return;
    }
    if (this.blobClient) {
      try {
        await this.blobClient.upload(hash, data);
      } catch (e) {
        console.warn(`[replica] upload of ${path} failed, will retry on next change`, e);
        return;
      }
    }
    this.doc.transact(() => {
      this.blobs.set(path, { hash, size: data.byteLength, mtime: file.stat.mtime });
    }, this.localOrigin);
    this.markSynced(path, hash);
  }

  private removeFromDoc(path: string): void {
    this.doc.transact(() => {
      if (this.texts.has(path)) this.texts.delete(path);
      if (this.blobs.has(path)) this.blobs.delete(path);
    }, this.localOrigin);
    this.forgetSynced(path);
  }

  private moveInDoc(oldPath: string, newPath: string): void {
    const t = this.texts.get(oldPath);
    const b = this.blobs.get(oldPath);
    this.doc.transact(() => {
      if (t) {
        this.texts.set(newPath, new Y.Text(t.toString()));
        this.texts.delete(oldPath);
      }
      if (b) {
        this.blobs.set(newPath, { ...b });
        this.blobs.delete(oldPath);
      }
    }, this.localOrigin);
    const h = this.state.synced[oldPath];
    this.forgetSynced(oldPath);
    if (h) this.markSynced(newPath, h);
  }

  private docPathsWithPrefix(prefix: string): string[] {
    const out: string[] = [];
    for (const k of this.texts.keys()) if (k.startsWith(prefix)) out.push(k);
    for (const k of this.blobs.keys()) if (k.startsWith(prefix)) out.push(k);
    return out;
  }

  // ---------------------------------------------------------------- doc -> local

  private async applyRemoteTextDelta(path: string, ytext: Y.Text, delta: Y.YTextEvent["delta"]): Promise<void> {
    const editors = this.editorsFor(path);
    const target = ytext.toString();
    if (editors.length === 0) {
      await this.writeTextToDisk(path, target);
      return;
    }
    const editor = editors[0];
    let offset = 0;
    for (const op of delta) {
      if (op.retain !== undefined) {
        offset += op.retain;
      } else if (op.delete !== undefined) {
        editor.replaceRange("", editor.offsetToPos(offset), editor.offsetToPos(offset + op.delete));
      } else if (op.insert !== undefined && typeof op.insert === "string") {
        editor.replaceRange(op.insert, editor.offsetToPos(offset));
        offset += op.insert.length;
      }
    }
    if (editor.getValue() !== target) {
      // Delta did not line up with the editor (edits raced); fall back to a full replace.
      const cursor = editor.getCursor();
      editor.setValue(target);
      editor.setCursor(cursor);
    }
    this.markSynced(path, hashText(target));
  }

  private async applyRemoteBlob(path: string, meta: BlobMeta): Promise<void> {
    if (!this.shouldSync(path)) return;
    if (this.state.synced[path] === meta.hash) return;
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      const localHash = await sha256Hex(await this.app.vault.readBinary(existing));
      if (localHash === meta.hash) {
        this.markSynced(path, meta.hash);
        return;
      }
      const lastSynced = this.state.synced[path];
      if (lastSynced && lastSynced !== localHash) {
        await this.writeConflictCopy(path, await this.app.vault.readBinary(existing));
      }
    }
    if (!this.blobClient) return;
    try {
      const data = await this.blobClient.download(meta.hash);
      if (!data) {
        console.warn(`[replica] blob for ${path} not on server yet`);
        return;
      }
      await this.writeBinaryToDisk(path, data, meta.hash);
    } catch (e) {
      console.warn(`[replica] download of ${path} failed`, e);
    }
  }

  private installObservers(): void {
    const onTexts = (events: Y.YEvent<any>[], txn: Y.Transaction) => {
      if (txn.origin === this.localOrigin) return;
      for (const ev of events) {
        if (ev.target === this.texts) {
          const mapEv = ev as Y.YMapEvent<Y.Text>;
          for (const [path, change] of mapEv.changes.keys) {
            if (!this.shouldSync(path)) continue;
            if (change.action === "delete") {
              void this.deleteFromDisk(path);
            } else {
              const ytext = this.texts.get(path);
              if (ytext) void this.applyRemoteTextReplace(path, ytext.toString(), change.action === "update");
            }
          }
        } else if (ev.target instanceof Y.Text && ev.path.length === 1) {
          const path = String(ev.path[0]);
          if (!this.shouldSync(path)) continue;
          void this.applyRemoteTextDelta(path, ev.target, (ev as Y.YTextEvent).delta);
        }
      }
    };
    this.texts.observeDeep(onTexts);
    this.unsubscribers.push(() => this.texts.unobserveDeep(onTexts));

    const onBlobs = (ev: Y.YMapEvent<BlobMeta>, txn: Y.Transaction) => {
      if (txn.origin === this.localOrigin) return;
      for (const [path, change] of ev.changes.keys) {
        if (!this.shouldSync(path)) continue;
        if (change.action === "delete") void this.deleteFromDisk(path);
        else {
          const meta = this.blobs.get(path);
          if (meta) void this.applyRemoteBlob(path, meta);
        }
      }
    };
    this.blobs.observe(onBlobs);
    this.unsubscribers.push(() => this.blobs.unobserve(onBlobs));
  }

  /** A whole Y.Text was added or replaced remotely (new file, rename, or concurrent create). */
  private async applyRemoteTextReplace(path: string, content: string, replaced: boolean): Promise<void> {
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      const local = await this.app.vault.read(existing);
      if (local === content) {
        this.markSynced(path, hashText(content));
        return;
      }
      if (replaced || this.state.synced[path] !== hashText(local)) {
        await this.writeConflictCopy(path, local);
      }
    }
    const editors = this.editorsFor(path);
    if (editors.length > 0) {
      const cursor = editors[0].getCursor();
      editors[0].setValue(content);
      editors[0].setCursor(cursor);
      this.markSynced(path, hashText(content));
    } else {
      await this.writeTextToDisk(path, content);
    }
  }

  // ---------------------------------------------------------------- vault listeners

  private installVaultListeners(): void {
    const v = this.app.vault;
    const w = this.app.workspace;

    const refs = [
      v.on("create", (f) => void this.onLocalChanged(f)),
      v.on("modify", (f) => void this.onLocalChanged(f)),
      v.on("delete", (f) => this.onLocalDeleted(f)),
      v.on("rename", (f, oldPath) => this.onLocalRenamed(f, oldPath)),
      w.on("editor-change", (editor, info) => {
        const file = info.file;
        if (!file || !this.isText(file.path) || !this.shouldSync(file.path)) return;
        this.scheduleEditorPush(file.path, editor);
      }),
      w.on("active-leaf-change", () => {
        const f = w.getActiveFile();
        this.setActiveFile(f ? f.path : null);
      }),
    ];
    for (const r of refs) this.unsubscribers.push(() => this.app.vault.offref(r));
  }

  private scheduleEditorPush(path: string, editor: Editor): void {
    const prev = this.editorTimers.get(path);
    if (prev) clearTimeout(prev);
    this.editorTimers.set(
      path,
      setTimeout(() => {
        this.editorTimers.delete(path);
        this.pushLocalText(path, editor.getValue());
      }, this.settings.liveDebounceMs),
    );
  }

  private async onLocalChanged(f: TAbstractFile): Promise<void> {
    if (!(f instanceof TFile) || this.stopped || !this.shouldSync(f.path)) return;
    if (this.isText(f.path)) {
      // If an editor is open the editor-change path already handles it; still verify disk content.
      const content = await this.app.vault.read(f);
      this.pushLocalText(f.path, content);
    } else {
      await this.pushLocalBlob(f.path, f);
    }
  }

  private onLocalDeleted(f: TAbstractFile): void {
    if (this.stopped) return;
    if (f instanceof TFolder) {
      for (const p of this.docPathsWithPrefix(f.path + "/")) this.removeFromDoc(p);
    } else if (this.shouldSync(f.path)) {
      this.removeFromDoc(f.path);
    }
  }

  private onLocalRenamed(f: TAbstractFile, oldPath: string): void {
    if (this.stopped) return;
    if (f instanceof TFolder) {
      for (const p of this.docPathsWithPrefix(oldPath + "/")) {
        this.moveInDoc(p, f.path + p.slice(oldPath.length));
      }
      return;
    }
    const oldOk = this.shouldSync(oldPath);
    const newOk = this.shouldSync(f.path);
    if (oldOk && newOk) this.moveInDoc(oldPath, f.path);
    else if (oldOk) this.removeFromDoc(oldPath);
    else if (newOk) void this.onLocalChanged(f);
  }

  // ---------------------------------------------------------------- reconcile

  /**
   * Three-way compare of every file: local disk, shared doc, and the hash we recorded the
   * last time the two agreed. Runs at startup and on demand. Idempotent.
   */
  async reconcileAll(): Promise<void> {
    const localFiles = new Map<string, TFile>();
    for (const f of this.app.vault.getFiles()) {
      if (this.shouldSync(f.path)) localFiles.set(f.path, f);
    }

    // Local files first.
    for (const [path, file] of localFiles) {
      try {
        if (this.isText(path)) await this.reconcileText(path, file);
        else await this.reconcileBlob(path, file);
      } catch (e) {
        console.error(`[replica] reconcile failed for ${path}`, e);
      }
    }

    // Then entries in the doc that are missing locally.
    for (const path of [...this.texts.keys()]) {
      if (localFiles.has(path) || !this.shouldSync(path)) continue;
      if (this.state.synced[path]) {
        // We had it and it is gone locally: deleted while offline.
        this.removeFromDoc(path);
      } else {
        const t = this.texts.get(path);
        if (t) await this.writeTextToDisk(path, t.toString());
      }
    }
    for (const path of [...this.blobs.keys()]) {
      if (localFiles.has(path) || !this.shouldSync(path)) continue;
      if (this.state.synced[path]) {
        this.removeFromDoc(path);
      } else {
        const m = this.blobs.get(path);
        if (m) await this.applyRemoteBlob(path, m);
      }
    }
    await this.persistence.saveState(this.state);
  }

  private async reconcileText(path: string, file: TFile): Promise<void> {
    const local = await this.app.vault.read(file);
    const localHash = hashText(local);
    const ytext = this.texts.get(path);
    const lastSynced = this.state.synced[path];

    if (!ytext) {
      if (lastSynced) {
        // Was in the doc before, now gone: deleted remotely while we were offline.
        await this.deleteFromDisk(path);
      } else {
        this.pushLocalText(path, local);
      }
      return;
    }

    const remote = ytext.toString();
    if (remote === local) {
      this.markSynced(path, localHash);
      return;
    }
    if (lastSynced === localHash) {
      // Only the remote side changed.
      await this.writeTextToDisk(path, remote);
    } else if (lastSynced === hashText(remote)) {
      // Only we changed.
      this.pushLocalText(path, local);
    } else {
      // Both changed, or first contact with differing content: keep both.
      await this.writeConflictCopy(path, local);
      await this.writeTextToDisk(path, remote);
    }
  }

  private async reconcileBlob(path: string, file: TFile): Promise<void> {
    const meta = this.blobs.get(path);
    const lastSynced = this.state.synced[path];
    if (!meta) {
      if (lastSynced) await this.deleteFromDisk(path);
      else await this.pushLocalBlob(path, file);
      return;
    }
    const localHash = await sha256Hex(await this.app.vault.readBinary(file));
    if (localHash === meta.hash) {
      this.markSynced(path, localHash);
      return;
    }
    if (lastSynced === localHash) {
      await this.applyRemoteBlob(path, meta);
    } else if (lastSynced === meta.hash) {
      await this.pushLocalBlob(path, file);
    } else {
      // Both changed: newer mtime wins, the loser is kept as a conflict copy.
      if (file.stat.mtime > meta.mtime) {
        await this.pushLocalBlob(path, file);
      } else {
        await this.writeConflictCopy(path, await this.app.vault.readBinary(file));
        await this.applyRemoteBlob(path, meta);
      }
    }
  }
}

function pad(n: number): string {
  return n < 10 ? "0" + n : String(n);
}

export { normalizePath };
