import { Modal, Notice, Setting } from "obsidian";
import { SamsynApi } from "./api";
import type SamsynPlugin from "./main";
import { renderQr, setupLink } from "./qr";
import { serverUrls, slugify } from "./settings";
import { copyText } from "./sharing-modals";

/** Issue a token for another device and show it as a QR code / deep link. */
export class DeviceSetupModal extends Modal {
  private label = "";

  constructor(private plugin: SamsynPlugin) {
    super(plugin.app);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: "Set up another device" });
    contentEl.createEl("p", {
      cls: "samsyn-muted",
      text: "Creates a token just for that device (revoke it later without touching this one) and shows a QR code. Scan it with the phone's camera: Obsidian opens and connects to the same server and vault.",
    });
    new Setting(contentEl)
      .setName("Device name")
      .setDesc("Used as the token's label, e.g. “iPhone”.")
      .addText((t) => t.setPlaceholder("iPhone").onChange((v) => (this.label = v)));
    const out = contentEl.createDiv();
    const msg = contentEl.createDiv({ cls: "samsyn-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Create token and QR")
        .setCta()
        .onClick(async () => {
          b.setDisabled(true);
          try {
            const s = this.plugin.settings;
            const r = await new SamsynApi(s).createToken(this.label.trim() || "device");
            const link = setupLink(s.serverUrl, r.token, s.vaultId, r.user);
            out.empty();
            renderQr(out, link, `Scan with the other device. Anyone who scans this gets access as “${r.user}”; revoke the token “${r.label}” under Devices if it leaks.`);
            new Setting(out)
              .addButton((x) => x.setButtonText("Copy link").onClick(() => void copyText(link, "Setup link")))
              .addButton((x) => x.setButtonText("Copy token only").onClick(() => void copyText(r.token, "Token")));
            if (!/^https:\/\//i.test(serverUrls(s.serverUrl)?.http ?? "")) {
              out.createDiv({ cls: "samsyn-error", text: "Your server URL is not https; a phone will not be able to connect to it." });
            }
          } catch (e) {
            msg.setText((e as Error).message);
            b.setDisabled(false);
          }
        }),
    );
  }

  onClose(): void {
    this.contentEl.empty();
  }
}

/** A setup deep link arrived (QR scanned): confirm before overwriting the connection settings. */
export class SetupConfirmModal extends Modal {
  constructor(
    private plugin: SamsynPlugin,
    private params: { server: string; token: string; vault: string; user?: string },
  ) {
    super(plugin.app);
  }

  onOpen(): void {
    const { contentEl } = this;
    const { server, token, vault, user } = this.params;
    const current = this.plugin.settings;
    const replacing = !!current.serverUrl && (current.serverUrl !== server || current.vaultId !== vault);
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: "Connect this device?" });
    const row = (label: string, value: string) => {
      const r = contentEl.createDiv({ cls: "samsyn-diag-row" });
      r.createSpan({ cls: "samsyn-diag-label", text: label });
      r.createSpan({ cls: "samsyn-diag-value", text: value });
    };
    row("Server", server);
    row("Vault on server", vault);
    if (user) row("User", user);
    row("Token", token.slice(0, 6) + "…");
    if (replacing) {
      contentEl.createDiv({
        cls: "samsyn-error",
        text: `This replaces the current connection (${current.serverUrl}, vault “${current.vaultId}”). This vault's files will be merged with what is in “${vault}” on the new server.`,
      });
    }
    const urls = serverUrls(server);
    if (!urls) contentEl.createDiv({ cls: "samsyn-error", text: "The server URL in this link is not valid." });
    new Setting(contentEl)
      .addButton((b) => b.setButtonText("Cancel").onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText("Connect")
          .setCta()
          .setDisabled(!urls)
          .onClick(async () => {
            b.setDisabled(true);
            current.serverUrl = server;
            current.token = token;
            current.vaultId = slugify(vault);
            current.enabled = true;
            await this.plugin.saveSettings();
            // The first sync downloads the whole vault; do not keep the dialog open for it.
            this.close();
            new Notice(`Samsyn: connecting to ${server}. The first sync can take a while; progress is on the Server tab.`, 8000);
            void this.plugin
              .restartEngine()
              .then(() => new Notice("Samsyn: first sync done"))
              .catch((e) => new Notice(`Samsyn: connecting failed (${(e as Error).message})`, 10_000));
            this.plugin.settingTab?.show("server");
          }),
      );
  }

  onClose(): void {
    this.contentEl.empty();
  }
}
