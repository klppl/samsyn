import { App, Notice, PluginSettingTab, Setting, requestUrl } from "obsidian";
import type ReplicaPlugin from "./main";

export interface ReplicaSettings {
  enabled: boolean;
  serverUrl: string;
  token: string;
  vaultId: string;
  deviceName: string;
  textExtensions: string;
  syncBinary: boolean;
  excludePaths: string;
  liveDebounceMs: number;
}

export const DEFAULT_SETTINGS: ReplicaSettings = {
  enabled: true,
  serverUrl: "",
  token: "",
  vaultId: "",
  deviceName: "",
  textExtensions: "md, txt",
  syncBinary: true,
  excludePaths: "",
  liveDebounceMs: 150,
};

export function slugify(name: string): string {
  const s = name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
  return s || "vault";
}

export function randomSuffix(len = 4): string {
  const chars = "abcdefghjkmnpqrstuvwxyz23456789";
  let out = "";
  for (let i = 0; i < len; i++) out += chars[Math.floor(Math.random() * chars.length)];
  return out;
}

/** Normalise user-entered server URL into http(s) base and ws(s) base. */
export function serverUrls(raw: string): { http: string; ws: string } | null {
  let s = raw.trim().replace(/\/+$/, "");
  if (!s) return null;
  if (!/^[a-z]+:\/\//i.test(s)) s = "http://" + s;
  let http: string;
  if (s.startsWith("ws://")) http = "http://" + s.slice(5);
  else if (s.startsWith("wss://")) http = "https://" + s.slice(6);
  else http = s;
  const ws = http.startsWith("https://") ? "wss://" + http.slice(8) : "ws://" + http.slice(7);
  return { http, ws: ws + "/ws" };
}

export class ReplicaSettingTab extends PluginSettingTab {
  constructor(app: App, private plugin: ReplicaPlugin) {
    super(app, plugin);
  }

  display(): void {
    const { containerEl } = this;
    containerEl.empty();
    const s = this.plugin.settings;

    new Setting(containerEl).setName("Replica").setHeading();

    new Setting(containerEl)
      .setName("Enable sync")
      .setDesc("Turn live synchronisation on or off for this device.")
      .addToggle((t) =>
        t.setValue(s.enabled).onChange(async (v) => {
          s.enabled = v;
          await this.plugin.saveSettings();
          await this.plugin.restartEngine();
        }),
      );

    new Setting(containerEl)
      .setName("Server URL")
      .setDesc("Address of your replica-server, e.g. https://sync.example.com or http://192.168.1.10:8787")
      .addText((t) =>
        t
          .setPlaceholder("http://localhost:8787")
          .setValue(s.serverUrl)
          .onChange(async (v) => {
            s.serverUrl = v.trim();
            await this.plugin.saveSettings();
          }),
      );

    new Setting(containerEl)
      .setName("Token")
      .setDesc("The auth token printed by the server on first start.")
      .addText((t) => {
        t.inputEl.type = "password";
        t.setValue(s.token).onChange(async (v) => {
          s.token = v.trim();
          await this.plugin.saveSettings();
        });
      });

    new Setting(containerEl)
      .setName("Vault ID")
      .setDesc("Devices with the same vault ID on the same server share one vault. Defaults to the vault name.")
      .addText((t) =>
        t.setValue(s.vaultId).onChange(async (v) => {
          s.vaultId = slugify(v);
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Device name")
      .setDesc("Shown to other devices and used in conflict file names.")
      .addText((t) =>
        t.setValue(s.deviceName).onChange(async (v) => {
          s.deviceName = v.trim() || s.deviceName;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Connect")
      .setDesc("Apply the settings above and (re)connect to the server.")
      .addButton((b) =>
        b.setButtonText("Test connection").onClick(async () => {
          const urls = serverUrls(s.serverUrl);
          if (!urls) {
            new Notice("Replica: enter a server URL first");
            return;
          }
          try {
            const res = await requestUrl({
              url: `${urls.http}/health`,
              headers: { Authorization: `Bearer ${s.token}` },
              throw: false,
            });
            if (res.status === 200) new Notice(`Replica: server OK (v${res.json?.version ?? "?"})`);
            else new Notice(`Replica: server responded ${res.status}`);
          } catch (e) {
            new Notice(`Replica: cannot reach server (${(e as Error).message})`);
          }
        }),
      )
      .addButton((b) =>
        b
          .setButtonText("Reconnect")
          .setCta()
          .onClick(async () => {
            await this.plugin.restartEngine();
            new Notice("Replica: reconnecting");
          }),
      );

    new Setting(containerEl).setName("What to sync").setHeading();

    new Setting(containerEl)
      .setName("Text file extensions")
      .setDesc("Comma separated. These are merged character by character. Everything else is synced as a whole file.")
      .addText((t) =>
        t.setValue(s.textExtensions).onChange(async (v) => {
          s.textExtensions = v;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Sync attachments and other files")
      .setDesc("Images, PDFs and other non-text files. Last writer wins.")
      .addToggle((t) =>
        t.setValue(s.syncBinary).onChange(async (v) => {
          s.syncBinary = v;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Exclude paths")
      .setDesc("Comma separated folder or file prefixes to skip, e.g. Private/, Scratch.md")
      .addText((t) =>
        t.setValue(s.excludePaths).onChange(async (v) => {
          s.excludePaths = v;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl).setName("Advanced").setHeading();

    new Setting(containerEl)
      .setName("Live typing delay (ms)")
      .setDesc("How long to wait after a keystroke before sending changes. Lower is more live.")
      .addSlider((sl) =>
        sl
          .setLimits(50, 2000, 50)
          .setValue(s.liveDebounceMs)
          .setDynamicTooltip()
          .onChange(async (v) => {
            s.liveDebounceMs = v;
            await this.plugin.saveSettings();
          }),
      );

    new Setting(containerEl)
      .setName("Full reconcile")
      .setDesc("Compare every file against the shared document and fix any drift.")
      .addButton((b) =>
        b.setButtonText("Reconcile now").onClick(async () => {
          await this.plugin.engine?.reconcileAll();
          new Notice("Replica: reconcile finished");
        }),
      );

    new Setting(containerEl)
      .setName("Reset local sync state")
      .setDesc("Forget the locally cached document and re-download from the server. Your files are not touched.")
      .addButton((b) =>
        b
          .setButtonText("Reset")
          .setWarning()
          .onClick(async () => {
            await this.plugin.resetLocalState();
            new Notice("Replica: local state reset, reconnecting");
          }),
      );
  }
}
