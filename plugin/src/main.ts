import { Notice, Platform, Plugin, setIcon } from "obsidian";
import { BlobClient } from "./blobs";
import { SyncEngine, EngineStatus, EngineTarget } from "./engine";
import { Persistence, deviceInstallId } from "./persistence";
import { MountModal, ShareModal } from "./sharing-modals";
import { DEFAULT_SETTINGS, GuestMount, ReplicaSettingTab, ReplicaSettings, randomSuffix, serverUrls, slugify } from "./settings";

export default class ReplicaPlugin extends Plugin {
  settings!: ReplicaSettings;
  /** Running engines: "home" (own server, whole vault) plus one per enabled guest mount. */
  engines = new Map<string, SyncEngine>();
  settingTab: ReplicaSettingTab | null = null;
  private statusEl!: HTMLElement;
  private restarting = false;

  /** The home engine (own server). Guest engines are in `engines` under their mount id. */
  get engine(): SyncEngine | null {
    return this.engines.get("home") ?? null;
  }

  guestEngine(mountId: string): SyncEngine | null {
    return this.engines.get(mountId) ?? null;
  }

  async onload(): Promise<void> {
    await this.loadSettings();

    this.statusEl = this.addStatusBarItem();
    this.statusEl.addClass("replica-status");
    this.statusEl.onClickEvent(() => {
      // @ts-expect-error setting API is not typed but exists on all platforms
      this.app.setting.open();
      // @ts-expect-error same as above
      this.app.setting.openTabById(this.manifest.id);
    });
    this.renderStatus("offline", 0);

    this.settingTab = new ReplicaSettingTab(this.app, this);
    this.addSettingTab(this.settingTab);

    this.addCommand({
      id: "share-current",
      name: "Share the current note or folder…",
      checkCallback: (checking) => {
        if (!this.settings.serverUrl) return false;
        const f = this.app.workspace.getActiveFile();
        if (!f) return false;
        if (!checking) new ShareModal(this, f.path).open();
        return true;
      },
    });
    this.addCommand({
      id: "mount-ticket",
      name: "Add a shared folder from a ticket…",
      checkCallback: (checking) => {
        if (!this.settings.serverUrl) return false;
        if (!checking) new MountModal(this).open();
        return true;
      },
    });
    this.addCommand({
      id: "open-sharing",
      name: "Open sharing settings",
      callback: () => this.settingTab?.show("sharing"),
    });

    this.addCommand({
      id: "reconnect",
      name: "Reconnect to server",
      callback: () => void this.restartEngine(),
    });
    this.addCommand({
      id: "reconcile",
      name: "Reconcile all files now",
      callback: async () => {
        await this.engine?.reconcileAll();
        new Notice("Replica: reconcile finished");
      },
    });
    this.addCommand({
      id: "apply-deletions",
      name: "Apply held-back deletions to the shared vault (danger)",
      callback: async () => {
        const n = this.engine?.pendingDeletions.length ?? 0;
        if (!this.engine || n === 0) {
          new Notice("Replica: nothing held back");
          return;
        }
        // The guard re-downloaded them; remove locally again first so reconcile sees them missing.
        for (const p of this.engine.pendingDeletions) {
          const f = this.app.vault.getAbstractFileByPath(p);
          if (f) await this.app.fileManager.trashFile(f);
        }
        await this.engine.reconcileAll({ allowMassDelete: true });
        new Notice(`Replica: removed ${n} files from the shared vault`);
      },
    });
    this.addCommand({
      id: "status",
      name: "Show sync status",
      callback: () => {
        const diags = [...this.engines.values()].map((e) => e.getDiagnostics());
        if (diags.length === 0) {
          new Notice("Replica: not running");
          return;
        }
        const lines: string[] = [];
        for (const d of diags) {
          lines.push(`${d.kind === "home" ? "Vault" : d.label}: ${d.status}${d.synced ? ", synced" : ""}, ${d.peers} other(s), ${d.textFiles} text + ${d.blobFiles} other files`);
          if (d.lastError) lines.push(`  error: ${d.lastError}`);
          if (d.hint) lines.push(`  ${d.hint}`);
          if (d.pendingDeletions > 0) lines.push(`  ${d.pendingDeletions} deletions held back by the safety guard`);
        }
        new Notice(lines.join("\n"), 10_000);
      },
    });
    this.addCommand({
      id: "toggle",
      name: "Toggle sync on/off",
      callback: async () => {
        this.settings.enabled = !this.settings.enabled;
        await this.saveSettings();
        await this.restartEngine();
        new Notice(`Replica: sync ${this.settings.enabled ? "enabled" : "disabled"}`);
      },
    });

    this.registerEvent(
      this.app.workspace.on("file-menu", (menu, file) => {
        if (!this.settings.serverUrl) return;
        if (this.settings.guestMounts.some((m) => file.path === m.mountPath || file.path.startsWith(m.mountPath + "/"))) return;
        menu.addItem((item) =>
          item
            .setTitle("Replica: Share…")
            .setIcon("share-2")
            .onClick(() => new ShareModal(this, file.path).open()),
        );
      }),
    );

    // Wait for the vault index so we do not get a 'create' event for every existing file.
    this.app.workspace.onLayoutReady(() => void this.startEngine());
  }

  async onunload(): Promise<void> {
    await this.stopEngine();
  }

  // ------------------------------------------------------------- engine control

  private persistence(scope: string): Persistence {
    return new Persistence(
      this.app.vault.adapter,
      `${this.app.vault.configDir}/plugins/${this.manifest.id}`,
      `${deviceInstallId(this.app)}|${scope}`,
    );
  }

  private guestRoots(): string[] {
    return this.settings.guestMounts.map((m) => m.mountPath.replace(/\/+$/, "") + "/");
  }

  private homeTarget(): EngineTarget | null {
    const urls = serverUrls(this.settings.serverUrl);
    if (!urls || !this.settings.vaultId) return null;
    return {
      kind: "home",
      id: "home",
      label: "your vault",
      wsBase: urls.ws,
      room: this.settings.vaultId,
      params: { token: this.settings.token },
      blobClient: BlobClient.forVault(urls.http, this.settings.vaultId, this.settings.token),
      root: "",
      readOnly: false,
      excludeRoots: this.guestRoots(),
    };
  }

  private guestTarget(m: GuestMount): EngineTarget | null {
    const urls = serverUrls(m.url);
    if (!urls) return null;
    return {
      kind: "guest",
      id: m.id,
      label: `“${m.name}” (shared with you)`,
      wsBase: urls.ws.replace(/\/ws$/, "/guest/ws"),
      room: m.shareId,
      params: { secret: m.secret },
      blobClient: BlobClient.forShare(urls.http, m.shareId, m.secret),
      root: m.mountPath.replace(/\/+$/, "") + "/",
      readOnly: m.access === "ro",
      excludeRoots: [],
    };
  }

  private async launch(target: EngineTarget, scope: string): Promise<void> {
    if (this.engines.has(target.id)) return;
    const engine = new SyncEngine(this.app, this.settings, this.persistence(scope), target, {
      onStatus: () => this.renderAggregateStatus(),
    });
    this.engines.set(target.id, engine);
    try {
      await engine.start();
    } catch (e) {
      console.error(`[replica] ${target.label} failed to start`, e);
      new Notice(`Replica: ${target.label} failed to start (${(e as Error).message})`);
      this.renderAggregateStatus();
    }
  }

  async startEngine(): Promise<void> {
    if (!this.settings.enabled) {
      this.renderStatus("disabled", 0);
      return;
    }
    const home = this.homeTarget();
    const guests = this.settings.guestMounts.filter((m) => m.enabled);
    if (!home && guests.length === 0) {
      this.renderStatus("disabled", 0);
      new Notice("Replica: set a server URL in settings, or add a shared folder from a ticket, to start syncing");
      return;
    }
    const starts: Promise<void>[] = [];
    if (home) starts.push(this.launch(home, this.settings.vaultId));
    for (const m of guests) {
      const t = this.guestTarget(m);
      if (t) starts.push(this.launch(t, `guest:${m.shareId}`));
    }
    await Promise.all(starts);
  }

  async stopEngine(): Promise<void> {
    const all = [...this.engines.values()];
    this.engines.clear();
    await Promise.all(all.map((e) => e.stop()));
  }

  /** Start syncing a guest mount that was just added (the home engine must re-exclude it). */
  async addGuestMount(m: GuestMount): Promise<void> {
    this.settings.guestMounts.push(m);
    await this.saveSettings();
    await this.restartEngine();
  }

  /** Stop syncing a guest mount and forget it. Files stay in the vault. */
  async removeGuestMount(id: string): Promise<void> {
    const m = this.settings.guestMounts.find((x) => x.id === id);
    this.settings.guestMounts = this.settings.guestMounts.filter((x) => x.id !== id);
    await this.saveSettings();
    const e = this.engines.get(id);
    if (e) {
      this.engines.delete(id);
      await e.stop();
    }
    if (m) await this.persistence(`guest:${m.shareId}`).clear();
    await this.restartEngine();
  }

  private renderAggregateStatus(): void {
    const diags = [...this.engines.values()].map((e) => e.getDiagnostics());
    if (diags.length === 0) {
      this.renderStatus("disabled", 0);
      return;
    }
    const rank: Record<EngineStatus, number> = { disabled: 0, connected: 1, connecting: 2, offline: 3 };
    let worst: EngineStatus = "disabled";
    let peers = 0;
    for (const d of diags) {
      if (rank[d.status] > rank[worst]) worst = d.status;
      peers += d.peers;
    }
    this.renderStatus(worst, peers);
  }

  async restartEngine(): Promise<void> {
    if (this.restarting) return;
    this.restarting = true;
    try {
      await this.stopEngine();
      await this.startEngine();
    } finally {
      this.restarting = false;
    }
  }

  async resetLocalState(): Promise<void> {
    await this.stopEngine();
    await this.persistence(this.settings.vaultId).clear();
    for (const m of this.settings.guestMounts) await this.persistence(`guest:${m.shareId}`).clear();
    await this.startEngine();
  }

  // ------------------------------------------------------------- settings

  async loadSettings(): Promise<void> {
    this.settings = Object.assign({}, DEFAULT_SETTINGS, (await this.loadData()) as Partial<ReplicaSettings>);
    let dirty = false;
    if (!this.settings.vaultId) {
      this.settings.vaultId = slugify(this.app.vault.getName());
      dirty = true;
    }
    if (!this.settings.deviceName) {
      this.settings.deviceName = `${Platform.isMobile ? "Mobile" : "Desktop"}-${randomSuffix()}`;
      dirty = true;
    }
    if (dirty) await this.saveSettings();
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.settings);
  }

  // ------------------------------------------------------------- status bar

  private renderStatus(status: EngineStatus, peers: number): void {
    const el = this.statusEl;
    el.empty();
    const icon = el.createSpan({ cls: "replica-status-icon" });
    const label = el.createSpan({ cls: "replica-status-label" });
    el.removeClass("is-connected", "is-offline", "is-connecting", "is-disabled");
    switch (status) {
      case "connected":
        setIcon(icon, "refresh-cw");
        label.setText(peers > 0 ? `Replica · ${peers} other${peers === 1 ? "" : "s"}` : "Replica");
        el.addClass("is-connected");
        el.setAttr("aria-label", "Replica: connected and live");
        break;
      case "connecting":
        setIcon(icon, "loader");
        label.setText("Replica…");
        el.addClass("is-connecting");
        el.setAttr("aria-label", "Replica: connecting");
        break;
      case "offline":
        setIcon(icon, "cloud-off");
        label.setText("Replica offline");
        el.addClass("is-offline");
        el.setAttr("aria-label", "Replica: offline, changes are queued locally");
        break;
      case "disabled":
        setIcon(icon, "pause");
        label.setText("Replica off");
        el.addClass("is-disabled");
        el.setAttr("aria-label", "Replica: sync disabled or not configured");
        break;
    }
  }
}
