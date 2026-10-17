import { Modal, Setting } from "obsidian";
import type { ConfigEntry, ConfigSync } from "./config-sync";
import type SamsynPlugin from "./main";

/** Choose which devices take part in syncing one plugin's settings. */
export class DevicePickerModal extends Modal {
  private all: boolean;
  private chosen: Set<string>;

  constructor(
    plugin: SamsynPlugin,
    private cs: ConfigSync,
    private pluginId: string,
    private pluginName: string,
    private entry: ConfigEntry,
    private onDone: () => void,
  ) {
    super(plugin.app);
    this.all = entry.devices === "all";
    this.chosen = new Set(entry.devices === "all" ? [] : entry.devices);
  }

  onOpen(): void {
    this.render();
  }

  private render(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: `Devices for “${this.pluginName}”` });
    contentEl.createEl("p", { cls: "samsyn-muted", text: "Which devices keep this plugin's settings in sync. A device also needs its own “Sync plugin settings on this device” switch on." });
    new Setting(contentEl)
      .setName("All devices")
      .setDesc("Including ones that join later.")
      .addToggle((t) =>
        t.setValue(this.all).onChange((v) => {
          this.all = v;
          this.render();
        }),
      );
    const devices = this.cs.knownDevices();
    if (devices.length === 0) contentEl.createDiv({ cls: "samsyn-empty", text: "No devices registered yet; they appear once they have synced with this selection on." });
    for (const d of devices) {
      const when = d.online ? "online now" : `last seen ${new Date(d.info.lastSeen).toLocaleString()}`;
      const row = new Setting(contentEl)
        .setClass("samsyn-grant")
        .setName(`${d.info.name}${d.self ? " (this device)" : ""}`)
        .setDesc(`${d.info.platform} · ${when}`);
      if (d.online) row.setClass("samsyn-online");
      row.addToggle((t) =>
        t
          .setValue(this.all || this.chosen.has(d.id))
          .setDisabled(this.all)
          .onChange((v) => {
            if (v) this.chosen.add(d.id);
            else this.chosen.delete(d.id);
          }),
      );
      if (!d.self && !d.online) {
        row.addExtraButton((b) =>
          b
            .setIcon("trash")
            .setTooltip("Forget this device (it re-registers if it syncs again)")
            .onClick(() => {
              this.cs.forgetDevice(d.id);
              this.chosen.delete(d.id);
              this.render();
            }),
        );
      }
    }
    new Setting(contentEl)
      .addButton((b) => b.setButtonText("Cancel").onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText("Save")
          .setCta()
          .onClick(() => {
            this.cs.set(this.pluginId, { ...this.entry, devices: this.all ? "all" : [...this.chosen] });
            this.onDone();
            this.close();
          }),
      );
  }

  onClose(): void {
    this.contentEl.empty();
  }
}
