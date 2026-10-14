import { Notice, Platform, Plugin, setIcon } from "obsidian";
import { SyncEngine, EngineStatus } from "./engine";
import { Persistence } from "./persistence";
import { DEFAULT_SETTINGS, ReplicaSettingTab, ReplicaSettings, randomSuffix, slugify } from "./settings";

export default class ReplicaPlugin extends Plugin {
  settings!: ReplicaSettings;
  engine: SyncEngine | null = null;
  private statusEl!: HTMLElement;
  private restarting = false;

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

    this.addSettingTab(new ReplicaSettingTab(this.app, this));

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
      id: "status",
      name: "Show sync status",
      callback: () => {
        const d = this.engine?.getDiagnostics();
        if (!d) {
          new Notice("Replica: not running");
          return;
        }
        const lines = [
          `Replica: ${d.status}${d.synced ? ", synced" : ""}, ${d.peers} other device(s)`,
          `${d.textFiles} text + ${d.blobFiles} other files`,
        ];
        if (d.lastError) lines.push(`Error: ${d.lastError}`);
        if (d.hint) lines.push(d.hint);
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

    // Wait for the vault index so we do not get a 'create' event for every existing file.
    this.app.workspace.onLayoutReady(() => void this.startEngine());
  }

  async onunload(): Promise<void> {
    await this.stopEngine();
  }

  // ------------------------------------------------------------- engine control

  private persistence(): Persistence {
    return new Persistence(this.app.vault.adapter, `${this.app.vault.configDir}/plugins/${this.manifest.id}`);
  }

  async startEngine(): Promise<void> {
    if (this.engine) return;
    if (!this.settings.enabled) {
      this.renderStatus("disabled", 0);
      return;
    }
    if (!this.settings.serverUrl) {
      this.renderStatus("disabled", 0);
      new Notice("Replica: set a server URL in settings to start syncing");
      return;
    }
    const engine = new SyncEngine(this.app, this.settings, this.persistence(), {
      onStatus: (s, peers) => this.renderStatus(s, peers),
    });
    this.engine = engine;
    try {
      await engine.start();
    } catch (e) {
      console.error("[replica] engine failed to start", e);
      new Notice(`Replica: failed to start (${(e as Error).message})`);
      this.renderStatus("offline", 0);
    }
  }

  async stopEngine(): Promise<void> {
    const e = this.engine;
    this.engine = null;
    if (e) await e.stop();
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
    await this.persistence().clear();
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
