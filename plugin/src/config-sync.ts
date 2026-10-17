import { App, Notice, Platform } from "obsidian";
import * as Y from "yjs";
import type { BlobMeta, SyncEngine } from "./engine";
import { sha256Hex } from "./hash";
import type { SamsynSettings } from "./settings";

/**
 * Which plugins' settings travel between devices, and to which devices. Lives in the shared
 * document (map `config`), so any device can change it. `devices` is "all" or device names.
 */
export interface ConfigEntry {
  devices: "all" | string[];
  /** Also sync main.js / manifest.json / styles.css, so the plugin itself follows. */
  code: boolean;
}

/** A device that has synced this vault; registered by each device itself, keyed by install id. */
export interface DeviceInfo {
  name: string;
  platform: "desktop" | "mobile";
  lastSeen: number;
}

const SETTINGS_FILE = "data.json";
const HEARTBEAT_MS = 5 * 60_000;
const CODE_FILES = ["manifest.json", "main.js", "styles.css"];
const POLL_MS = 10_000;

/**
 * Syncs `.obsidian/plugins/<id>/…` for selected plugins. These files are outside the vault's
 * file index (no TFile, no vault events), so they are read and written through the adapter,
 * stored as whole-file blobs (last writer wins, never merged: a half-merged JSON would break
 * the plugin) and watched by polling their mtime. The sync engine itself ignores every key
 * under `.obsidian/`, so nothing here can collide with note syncing.
 */
export class ConfigSync {
  private config: Y.Map<ConfigEntry>;
  private devices: Y.Map<DeviceInfo>;
  private timer: ReturnType<typeof setInterval> | null = null;
  private heartbeat: ReturnType<typeof setInterval> | null = null;
  private unsubscribers: Array<() => void> = [];
  private seen = new Map<string, { mtime: number; hash: string }>();
  private busy = false;
  private stopped = false;
  private readonly origin = { samsyn: "config" };

  constructor(
    private app: App,
    private settings: SamsynSettings,
    private engine: SyncEngine,
  ) {
    this.config = engine.doc.getMap<ConfigEntry>("config");
    this.devices = engine.doc.getMap<DeviceInfo>("devices");
  }

  // ------------------------------------------------------------------ devices

  get installId(): string {
    return this.engine.deviceInstallId;
  }

  /** Every device that has synced this vault, this one first, then by name. */
  knownDevices(): Array<{ id: string; info: DeviceInfo; online: boolean; self: boolean }> {
    const online = new Set(this.engine.peerInstallIds());
    const out = [...this.devices.entries()].map(([id, info]) => ({ id, info, online: online.has(id), self: id === this.installId }));
    out.sort((a, b) => (a.self ? -1 : b.self ? 1 : a.info.name.localeCompare(b.info.name)));
    return out;
  }

  /** Register / refresh this device in the shared registry. */
  touchSelf(): void {
    const info: DeviceInfo = { name: this.settings.deviceName, platform: Platform.isMobile ? "mobile" : "desktop", lastSeen: Date.now() };
    const cur = this.devices.get(this.installId);
    if (cur && cur.name === info.name && cur.platform === info.platform && Date.now() - cur.lastSeen < HEARTBEAT_MS / 2) return;
    this.engine.doc.transact(() => this.devices.set(this.installId, info), this.origin);
  }

  /** Drop a device that is gone for good (e.g. a wiped phone). It re-registers if it comes back. */
  forgetDevice(id: string): void {
    this.engine.doc.transact(() => {
      this.devices.delete(id);
      for (const [pid, e] of this.config.entries()) {
        if (e.devices !== "all" && e.devices.includes(id)) this.config.set(pid, { ...e, devices: e.devices.filter((d) => d !== id) });
      }
    }, this.origin);
  }

  /** Human-readable summary of an entry's device selection. */
  describeDevices(e: ConfigEntry): string {
    if (e.devices === "all") return "all devices";
    const names = e.devices.map((d) => this.devices.get(d)?.name ?? d);
    return names.length === 0 ? "no devices" : names.join(", ");
  }

  // ------------------------------------------------------------------ selection

  entries(): Array<[string, ConfigEntry]> {
    return [...this.config.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }

  get(id: string): ConfigEntry | undefined {
    return this.config.get(id);
  }

  set(id: string, entry: ConfigEntry | null): void {
    this.engine.doc.transact(() => {
      if (entry) this.config.set(id, entry);
      else this.config.delete(id);
    }, this.origin);
  }

  private get deviceName(): string {
    return this.settings.deviceName;
  }

  /** Does this device take part in syncing plugin `id`? (Entries made before the device
   *  registry existed may hold device names; those still match.) */
  participating(id: string): boolean {
    if (!this.settings.configSync.enabled) return false;
    const e = this.config.get(id);
    if (!e) return false;
    return e.devices === "all" || e.devices.includes(this.installId) || e.devices.includes(this.deviceName);
  }

  private filesFor(id: string): string[] {
    const e = this.config.get(id);
    const dir = `${this.app.vault.configDir}/plugins/${id}`;
    const files = [`${dir}/${SETTINGS_FILE}`];
    if (e?.code) for (const f of CODE_FILES) files.push(`${dir}/${f}`);
    return files;
  }

  private pluginOf(key: string): string | null {
    const prefix = `${this.app.vault.configDir}/plugins/`;
    if (!key.startsWith(prefix)) return null;
    const rest = key.slice(prefix.length);
    const slash = rest.indexOf("/");
    if (slash < 0) return null;
    const id = rest.slice(0, slash);
    const file = rest.slice(slash + 1);
    if (file !== SETTINGS_FILE && !CODE_FILES.includes(file)) return null;
    return id;
  }

  // ------------------------------------------------------------------ lifecycle

  start(): void {
    this.stopped = false;
    const onBlobs = (ev: Y.YMapEvent<BlobMeta>, txn: Y.Transaction) => {
      if (txn.origin === this.origin) return;
      for (const [key, change] of ev.changes.keys) {
        const id = this.pluginOf(key);
        if (!id || !this.participating(id)) continue;
        if (change.action === "delete") continue; // never delete a plugin's files because of a remote removal
        const meta = this.engine.blobs.get(key);
        if (meta) void this.pull(key, meta, id);
      }
    };
    this.engine.blobs.observe(onBlobs);
    this.unsubscribers.push(() => this.engine.blobs.unobserve(onBlobs));

    const onConfig = () => void this.reconcileAll();
    this.config.observe(onConfig);
    this.unsubscribers.push(() => this.config.unobserve(onConfig));

    // Obsidian emits an undocumented "raw" event for config-dir writes on some platforms;
    // polling covers the rest.
    try {
      const v = this.app.vault as unknown as { on: (n: string, f: (p: string) => void) => unknown; offref: (r: unknown) => void };
      const ref = v.on("raw", (path: string) => {
        const id = this.pluginOf(path);
        if (id && this.participating(id)) void this.push(path, id);
      });
      this.unsubscribers.push(() => v.offref(ref));
    } catch {
      /* not available */
    }

    this.timer = setInterval(() => void this.scanLocal(), POLL_MS);
    this.touchSelf();
    this.heartbeat = setInterval(() => this.touchSelf(), HEARTBEAT_MS);
    void this.reconcileAll();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = null;
    for (const u of this.unsubscribers) u();
    this.unsubscribers = [];
  }

  // ------------------------------------------------------------------ sync

  /** Compare every participating file with the document; newer modification time wins. */
  async reconcileAll(): Promise<void> {
    if (this.busy || this.stopped) return;
    this.busy = true;
    try {
      for (const [id] of this.entries()) {
        if (!this.participating(id)) continue;
        for (const key of this.filesFor(id)) {
          const meta = this.engine.blobs.get(key);
          const local = await this.statLocal(key);
          if (!meta && local) await this.push(key, id);
          else if (meta && !local) await this.pull(key, meta, id);
          else if (meta && local) {
            if (local.hash === meta.hash) this.seen.set(key, local);
            else if (local.mtime > meta.mtime) await this.push(key, id);
            else await this.pull(key, meta, id);
          }
        }
      }
    } finally {
      this.busy = false;
    }
  }

  private async scanLocal(): Promise<void> {
    if (this.busy || this.stopped) return;
    for (const [id] of this.entries()) {
      if (!this.participating(id)) continue;
      for (const key of this.filesFor(id)) {
        const stat = await this.app.vault.adapter.stat(key).catch(() => null);
        if (!stat || stat.type !== "file") continue;
        const prev = this.seen.get(key);
        if (!prev || prev.mtime !== stat.mtime) await this.push(key, id);
      }
    }
  }

  private async statLocal(key: string): Promise<{ mtime: number; hash: string } | null> {
    const stat = await this.app.vault.adapter.stat(key).catch(() => null);
    if (!stat || stat.type !== "file") return null;
    const data = await this.app.vault.adapter.readBinary(key);
    return { mtime: stat.mtime, hash: await sha256Hex(data) };
  }

  private async push(key: string, id: string): Promise<void> {
    if (this.stopped) return;
    const stat = await this.app.vault.adapter.stat(key).catch(() => null);
    if (!stat || stat.type !== "file") return;
    const data = await this.app.vault.adapter.readBinary(key);
    const hash = await sha256Hex(data);
    this.seen.set(key, { mtime: stat.mtime, hash });
    const current = this.engine.blobs.get(key);
    if (current?.hash === hash) return;
    const client = this.engine.getBlobClient();
    if (!client) return;
    try {
      await client.upload(hash, data);
    } catch (e) {
      console.warn(`[samsyn] uploading ${key} failed`, e);
      return;
    }
    this.engine.doc.transact(() => {
      this.engine.blobs.set(key, { hash, size: data.byteLength, mtime: stat.mtime });
    }, this.origin);
    console.info(`[samsyn] plugin settings pushed: ${key} (${id})`);
  }

  private async pull(key: string, meta: BlobMeta, id: string): Promise<void> {
    if (this.stopped) return;
    const local = await this.statLocal(key);
    if (local?.hash === meta.hash) {
      this.seen.set(key, local);
      return;
    }
    const client = this.engine.getBlobClient();
    if (!client) return;
    let data: ArrayBuffer | null = null;
    try {
      data = await client.download(meta.hash);
    } catch (e) {
      console.warn(`[samsyn] downloading ${key} failed`, e);
      return;
    }
    if (!data) return; // not on the server yet; the next reconcile or poll retries
    const dir = key.slice(0, key.lastIndexOf("/"));
    if (!(await this.app.vault.adapter.exists(dir))) await this.app.vault.adapter.mkdir(dir);
    await this.app.vault.adapter.writeBinary(key, data);
    const stat = await this.app.vault.adapter.stat(key).catch(() => null);
    this.seen.set(key, { mtime: stat?.mtime ?? meta.mtime, hash: meta.hash });
    console.info(`[samsyn] plugin settings pulled: ${key} (${id})`);
    this.offerReload(id, key.endsWith(SETTINGS_FILE));
  }

  private reloadOffered = new Map<string, number>();

  /** Plugins read data.json at load time, so a pulled change needs a reload to show. */
  private offerReload(id: string, settingsChanged: boolean): void {
    const last = this.reloadOffered.get(id) ?? 0;
    if (Date.now() - last < 5000) return;
    this.reloadOffered.set(id, Date.now());
    const plugins = (this.app as unknown as { plugins: { manifests: Record<string, { name: string }>; enabledPlugins: Set<string>; disablePlugin(id: string): Promise<void>; enablePlugin(id: string): Promise<void> } }).plugins;
    const name = plugins?.manifests?.[id]?.name ?? id;
    const installed = !!plugins?.manifests?.[id];
    const frag = document.createDocumentFragment();
    if (!installed) {
      frag.createDiv({ text: `Samsyn: files for the plugin “${id}” arrived from another device. Restart Obsidian, then enable it under Community plugins.` });
      new Notice(frag, 12_000);
      return;
    }
    frag.createDiv({ text: `Samsyn: ${settingsChanged ? "settings" : "files"} for “${name}” updated from another device. Reload it to apply.` });
    const btn = frag.createEl("button", { text: `Reload ${name}` });
    btn.onclick = async () => {
      notice.hide();
      try {
        if (plugins.enabledPlugins.has(id)) {
          await plugins.disablePlugin(id);
          await plugins.enablePlugin(id);
        }
        new Notice(`Samsyn: ${name} reloaded`);
      } catch (e) {
        new Notice(`Samsyn: reloading ${name} failed (${(e as Error).message}); restart Obsidian.`);
      }
    };
    const notice = new Notice(frag, 20_000);
  }
}
