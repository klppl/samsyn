import { AbstractInputSuggest, App, Modal, Notice, Setting, TAbstractFile, TFolder } from "obsidian";
import { SamsynApi, ShareRow, TicketInfo } from "./api";
import type SamsynPlugin from "./main";
import type { SharedRoot } from "./main";
import { mountLink, renderQr } from "./qr";
import { serverUrls } from "./settings";
import { decodeTicket, Ticket } from "./ticket";

export async function copyText(text: string, what = "Ticket"): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    new Notice(`${what} copied to clipboard`);
  } catch {
    new Notice(`Could not access the clipboard; select and copy the ${what.toLowerCase()} manually`);
  }
}

/** Vault path completion for the share dialog. */
class PathSuggest extends AbstractInputSuggest<TAbstractFile> {
  constructor(
    app: App,
    private input: HTMLInputElement,
  ) {
    super(app, input);
  }
  getSuggestions(query: string): TAbstractFile[] {
    const q = query.toLowerCase();
    return this.app.vault
      .getAllLoadedFiles()
      .filter((f) => f.path && f.path !== "/" && f.path.toLowerCase().includes(q))
      .sort((a, b) => a.path.length - b.path.length)
      .slice(0, 40);
  }
  renderSuggestion(f: TAbstractFile, el: HTMLElement): void {
    el.setText(f instanceof TFolder ? `${f.path}/` : f.path);
  }
  selectSuggestion(f: TAbstractFile): void {
    this.setValue(f.path);
    this.input.dispatchEvent(new Event("input"));
    this.close();
  }
}

/** "read & write", "read only" or "read only, except Inbox/, todo.md". */
export const accessText = (a: string, rwPaths: string[] = []) =>
  a === "ro" ? (rwPaths.length ? `read only, except ${rwPaths.join(", ")}` : "read only") : "read & write";

/** A share's path for display: "" is the whole vault. */
export const pathText = (p: string) => (p ? p : "the whole vault");

/** One path per line (or comma separated) from the writable-paths box. */
function parsePaths(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((p) => p.trim().replace(/^\/+/, ""))
    .filter((p) => p.length > 0);
}

/** The "Writable paths" box shared by the share and new-ticket dialogs; visible for read-only tickets. */
function addRwPathsSetting(el: HTMLElement, onChange: (v: string) => void): Setting {
  const s = new Setting(el)
    .setName("Still writable")
    .setDesc("Optional, for read-only tickets: folders or notes (relative to what is shared) the holder may edit anyway, one per line. A folder that does not exist yet is fine.")
    .addTextArea((t) => {
      t.setPlaceholder("Inbox\nMeetings/2026").onChange(onChange);
      t.inputEl.rows = 3;
      t.inputEl.addClass("samsyn-wide");
    });
  return s;
}

/** Shows a freshly issued ticket. Tickets are shown once; the server only stores a hash. */
function renderTicket(el: HTMLElement, ticket: string, title: string, hint: string): void {
  el.empty();
  el.createEl("h3", { text: title });
  el.createEl("p", { text: hint, cls: "samsyn-muted" });
  const ta = el.createEl("textarea", { cls: "samsyn-ticket-text" });
  ta.value = ticket;
  ta.readOnly = true;
  ta.rows = 4;
  ta.onclick = () => ta.select();
  const link = mountLink(ticket);
  new Setting(el)
    .addButton((b) => b.setButtonText("Copy ticket").setCta().onClick(() => void copyText(ticket)))
    .addButton((b) => b.setButtonText("Copy link").onClick(() => void copyText(link, "Link")))
    .addButton((b) => b.setButtonText("Done").onClick(() => el.closest(".modal-container")?.querySelector<HTMLElement>(".modal-close-button")?.click()));
  renderQr(el.createDiv(), link, "Scan with a phone: Obsidian opens with this ticket ready to mount. The link works on desktop too.");
}

/** Share a folder or a note: pick the path, get the first ticket. */
export class ShareModal extends Modal {
  private api: SamsynApi;
  private path: string;
  private wholeVault = false;
  private name = "";
  private access = "rw";
  private rwPaths = "";

  constructor(
    private plugin: SamsynPlugin,
    initialPath = "",
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.api = new SamsynApi(plugin.settings);
    this.path = initialPath;
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: "Share a folder or note" });
    contentEl.createEl("p", {
      cls: "samsyn-muted",
      text: "Whoever gets the ticket sees exactly this path, live, in their own vault. Nothing else in your vault is visible to them. One ticket covers one path; share another path with another ticket. Sharing the whole vault leaves no room for other shares in it.",
    });
    let pathInput: import("obsidian").TextComponent | null = null;
    new Setting(contentEl)
      .setName("Folder or note")
      .setDesc("A path in this vault, e.g. Projects/X or Notes/plan.md")
      .addText((t) => {
        pathInput = t;
        t.setPlaceholder("Projects/X").setValue(this.path).onChange((v) => (this.path = v));
        t.inputEl.addClass("samsyn-wide");
        new PathSuggest(this.app, t.inputEl);
      });
    new Setting(contentEl)
      .setName("Share the whole vault")
      .setDesc("Everything synced in this vault, including notes added later. Combine with a read-only ticket and a few writable folders to give someone a window into your vault with an inbox.")
      .addToggle((t) =>
        t.setValue(this.wholeVault).onChange((v) => {
          this.wholeVault = v;
          pathInput?.setDisabled(v);
        }),
      );
    new Setting(contentEl)
      .setName("Name")
      .setDesc("How the other person sees it. Defaults to the folder, note or vault name.")
      .addText((t) => t.setPlaceholder("optional").onChange((v) => (this.name = v)));
    let rwSetting: Setting | null = null;
    new Setting(contentEl)
      .setName("Access for the first ticket")
      .setDesc("Read-only people see your edits but cannot change anything, except in the paths you list below. You can issue more tickets later.")
      .addDropdown((d) =>
        d.addOptions({ rw: "Read & write", ro: "Read only" }).setValue(this.access).onChange((v) => {
          this.access = v;
          rwSetting?.settingEl.toggle(v === "ro");
        }),
      );
    rwSetting = addRwPathsSetting(contentEl, (v) => (this.rwPaths = v));
    rwSetting.settingEl.toggle(this.access === "ro");
    const msg = contentEl.createDiv({ cls: "samsyn-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Create share")
        .setCta()
        .onClick(async () => {
          const p = this.wholeVault ? "" : this.path.trim().replace(/^\/+|\/+$/g, "");
          if (!p && !this.wholeVault) {
            msg.setText("Enter a path, or switch on “Share the whole vault”.");
            return;
          }
          const rwPaths = this.access === "ro" ? parsePaths(this.rwPaths) : [];
          b.setDisabled(true);
          try {
            const r = await this.api.createShare(p, this.name.trim(), this.access, rwPaths);
            this.onDone?.();
            renderTicket(
              contentEl,
              r.ticket,
              `Sharing ${pathText(r.path ?? p)}`,
              `Send this ticket (${accessText(r.access ?? this.access, r.rw_paths ?? rwPaths)}) to the person you want to work with. It is shown once; you can always create a new one.`,
            );
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

/** Issue one more ticket for an existing share. */
export class NewTicketModal extends Modal {
  private api: SamsynApi;
  private label = "";
  private access = "rw";
  private rwPaths = "";

  constructor(
    private plugin: SamsynPlugin,
    private share: ShareRow,
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.api = new SamsynApi(plugin.settings);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: `New ticket for ${this.share.name}` });
    contentEl.createEl("p", { cls: "samsyn-muted", text: `${pathText(this.share.path)} · each person should get their own ticket, so you can revoke one without affecting the others.` });
    new Setting(contentEl)
      .setName("Label")
      .setDesc("Who this ticket is for, e.g. the person's name.")
      .addText((t) => t.setPlaceholder("anna").onChange((v) => (this.label = v)));
    let rwSetting: Setting | null = null;
    new Setting(contentEl).setName("Access").addDropdown((d) =>
      d.addOptions({ rw: "Read & write", ro: "Read only" }).setValue(this.access).onChange((v) => {
        this.access = v;
        rwSetting?.settingEl.toggle(v === "ro");
      }),
    );
    if (this.share.only_file === null) {
      rwSetting = addRwPathsSetting(contentEl, (v) => (this.rwPaths = v));
      rwSetting.settingEl.toggle(this.access === "ro");
    }
    const msg = contentEl.createDiv({ cls: "samsyn-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Create ticket")
        .setCta()
        .onClick(async () => {
          b.setDisabled(true);
          try {
            const rwPaths = this.access === "ro" ? parsePaths(this.rwPaths) : [];
            const r = await this.api.newTicket(this.share.id, this.access, this.label.trim(), rwPaths);
            this.onDone?.();
            renderTicket(contentEl, r.ticket, `Ticket "${r.label}"`, `${accessText(r.access, r.rw_paths)} access to ${pathText(this.share.path)}. Shown once.`);
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

/**
 * Paste a ticket, see what it is, choose where it goes, mount: either through your own server
 * (the server keeps a replica and bridges it into your vault) or as a guest (this plugin talks
 * to the owner's server directly; no server of your own needed).
 */
export class MountModal extends Modal {
  private api: SamsynApi;
  private ticket = "";
  private mountPath = "";
  private info: TicketInfo | null = null;
  private local: Ticket | null = null;
  private inspectTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private plugin: SamsynPlugin,
    private onDone?: () => void,
    private initialTicket = "",
  ) {
    super(plugin.app);
    this.api = new SamsynApi(plugin.settings);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: "Add a shared folder or note" });
    contentEl.createEl("p", { cls: "samsyn-muted", text: "Paste the ticket you received. The shared files appear in a folder of your choice and stay live in both directions." });
    const ta = contentEl.createEl("textarea", { cls: "samsyn-ticket-text" });
    ta.placeholder = "samsyn…";
    ta.rows = 3;
    const card = contentEl.createDiv({ cls: "samsyn-ticket-info" });
    const pathSetting = new Setting(contentEl).setName("Put it in").setDesc("A new or empty folder in this vault.");
    let pathInput: HTMLInputElement | null = null;
    pathSetting.addText((t) => {
      t.setPlaceholder("Shared/Name").onChange((v) => (this.mountPath = v));
      pathInput = t.inputEl;
    });
    const msg = contentEl.createDiv({ cls: "samsyn-error" });
    const hasServer = !!(this.plugin.settings.serverUrl && this.plugin.settings.token);
    let serverBtn: import("obsidian").ButtonComponent | null = null;
    let guestBtn: import("obsidian").ButtonComponent | null = null;
    const buttons = new Setting(contentEl);
    if (hasServer) {
      buttons.addButton((b) => {
        serverBtn = b;
        b.setButtonText("Mount via my server")
          .setCta()
          .setDisabled(true)
          .onClick(async () => {
            b.setDisabled(true);
            try {
              const r = await this.api.mount(this.ticket.trim(), this.mountPath.trim());
              new Notice(`Samsyn: mounted at ${r.mount_path} (${accessText(r.access, r.rw_paths)}). Files arrive shortly.`, 8000);
              this.onDone?.();
              this.close();
            } catch (e) {
              msg.setText((e as Error).message);
              b.setDisabled(false);
            }
          });
      });
    }
    buttons.addButton((b) => {
      guestBtn = b;
      b.setButtonText(hasServer ? "Mount as guest" : "Mount")
        .setDisabled(true)
        .onClick(async () => {
          b.setDisabled(true);
          try {
            await this.mountAsGuest();
            this.onDone?.();
            this.close();
          } catch (e) {
            msg.setText((e as Error).message);
            b.setDisabled(false);
          }
        });
      if (!hasServer) b.setCta();
    });
    contentEl.createDiv({
      cls: "samsyn-muted samsyn-small",
      text: hasServer
        ? "Via my server: your server keeps a copy and links to the owner's server, works on all your devices. As guest: this device talks to the owner's server directly; no account there needed, only this device."
        : "This device will talk to the owner's server directly. Nothing else in your vault is sent anywhere.",
    });

    const row = (label: string, value: string, cls = "") => {
      const r = card.createDiv({ cls: "samsyn-diag-row" });
      r.createSpan({ cls: "samsyn-diag-label", text: label });
      r.createSpan({ cls: "samsyn-diag-value " + cls, text: value });
    };

    const inspect = async () => {
      const t = this.ticket.trim();
      card.empty();
      msg.setText("");
      this.info = null;
      this.local = null;
      serverBtn?.setDisabled(true);
      guestBtn?.setDisabled(true);
      if (!t) return;
      // Decode locally first: works without any server and tells us if guest mode is possible.
      try {
        this.local = decodeTicket(t);
      } catch (e) {
        msg.setText((e as Error).message);
        return;
      }
      const local = this.local;
      row("Share", local.name);
      row("Access", accessText(local.access, local.rwPaths));
      if (!this.mountPath.trim() && pathInput) {
        this.mountPath = `Shared/${local.name}`;
        pathInput.value = this.mountPath;
      }
      if (local.url) {
        const insecure = local.url.startsWith("http://") && (this.app as any).isMobile;
        row("Guest access", insecure ? `${local.url} is plain http; mobile needs https` : `${local.url}`, insecure ? "is-bad" : "");
        guestBtn?.setDisabled(insecure);
      } else {
        row("Guest access", "not offered by this ticket (owner's server has no public URL)", "is-warn");
      }
      if (!hasServer) return;
      try {
        const info = await this.api.inspectTicket(t);
        if (this.ticket.trim() !== t) return; // superseded
        this.info = info;
        row("Via my server", info.same_server ? "same server as the owner" : `owner is on another server (${info.node_id.slice(0, 10)}…), linked peer-to-peer`);
        if (info.same_server && (info.access !== local.access || JSON.stringify(info.rw_paths ?? []) !== JSON.stringify(local.rwPaths))) {
          row("Access (server)", accessText(info.access, info.rw_paths ?? []));
        }
        if (info.from_own_vault) row("Owner", `you (vault "${info.from_own_vault}")`, "is-warn");
        if (info.valid === false) {
          row("Status", "invalid or revoked", "is-bad");
          guestBtn?.setDisabled(true);
          return;
        }
        if (!info.same_server && !info.p2p_available) {
          row("Status", "your server runs with p2p disabled; cannot mount this via your server", "is-warn");
          return;
        }
        serverBtn?.setDisabled(false);
      } catch (e) {
        msg.setText((e as Error).message);
      }
    };
    ta.addEventListener("input", () => {
      this.ticket = ta.value;
      if (this.inspectTimer) clearTimeout(this.inspectTimer);
      this.inspectTimer = setTimeout(() => void inspect(), 300);
    });
    if (this.initialTicket) {
      ta.value = this.initialTicket;
      this.ticket = this.initialTicket;
      void inspect();
    } else {
      ta.focus();
    }
  }

  private async mountAsGuest(): Promise<void> {
    const t = this.local;
    if (!t) throw new Error("paste a ticket first");
    if (!t.url) throw new Error("this ticket cannot be used as a guest; mount it via your own server instead");
    if (!serverUrls(t.url)) throw new Error("the ticket's server URL is not valid");
    const mountPath = this.mountPath.trim().replace(/^\/+|\/+$/g, "");
    if (!mountPath) throw new Error("choose a folder for the shared files");
    const s = this.plugin.settings;
    const prefix = mountPath + "/";
    for (const m of s.guestMounts) {
      const p = m.mountPath + "/";
      if (p.startsWith(prefix) || prefix.startsWith(p)) throw new Error(`overlaps the shared folder at ${m.mountPath}`);
      if (m.shareId === t.shareId) throw new Error(`“${m.name}” is already mounted at ${m.mountPath}`);
    }
    const existing = this.app.vault.getAbstractFileByPath(mountPath);
    if (existing && !(existing instanceof TFolder)) throw new Error(`${mountPath} is a file`);
    if (existing instanceof TFolder && existing.children.length > 0) throw new Error(`${mountPath} already contains files; choose an empty or new folder`);
    await this.plugin.addGuestMount({
      id: "g" + Math.random().toString(36).slice(2, 10),
      url: t.url,
      shareId: t.shareId,
      secret: t.secret,
      access: t.access === "ro" ? "ro" : "rw",
      rwPaths: t.access === "ro" ? t.rwPaths : [],
      name: t.name,
      mountPath,
      enabled: true,
    });
    new Notice(`Samsyn: “${t.name}” mounted at ${mountPath} as guest (${accessText(t.access, t.rwPaths)}). Files arrive shortly.`, 8000);
  }

  onClose(): void {
    if (this.inspectTimer) clearTimeout(this.inspectTimer);
    this.contentEl.empty();
  }
}

/** Move a mounted share to another folder. Done as an ordinary folder move, which the plugin
 *  follows (server mount: the server moves the keys; guest: the mount path is updated). */
export class MoveMountModal extends Modal {
  private target: string;

  constructor(
    private plugin: SamsynPlugin,
    private root: SharedRoot,
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.target = root.path;
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: `Move “${this.root.name}”` });
    contentEl.createEl("p", {
      cls: "samsyn-muted",
      text: `Currently at ${this.root.path}. The files move with it and stay shared. You can also just drag the folder in the file explorer; Samsyn follows.`,
    });
    new Setting(contentEl).setName("New folder").addText((t) => {
      t.setValue(this.target).onChange((v) => (this.target = v));
      t.inputEl.addClass("samsyn-wide");
      new PathSuggest(this.app, t.inputEl);
    });
    const msg = contentEl.createDiv({ cls: "samsyn-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Move")
        .setCta()
        .onClick(async () => {
          b.setDisabled(true);
          try {
            await this.plugin.moveMountFolder(this.root, this.target.trim());
            this.onDone?.();
            this.close();
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

/** Two-step confirmation for revoke / unshare / unmount. */
export class ConfirmModal extends Modal {
  constructor(
    app: App,
    private title: string,
    private body: string,
    private confirmText: string,
    private onConfirm: () => Promise<void> | void,
  ) {
    super(app);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("samsyn-modal");
    contentEl.createEl("h2", { text: this.title });
    contentEl.createEl("p", { text: this.body });
    new Setting(contentEl)
      .addButton((b) => b.setButtonText("Cancel").onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(this.confirmText)
          .setWarning()
          .onClick(async () => {
            b.setDisabled(true);
            try {
              await this.onConfirm();
              this.close();
            } catch (e) {
              new Notice(`Samsyn: ${(e as Error).message}`, 8000);
              b.setDisabled(false);
            }
          }),
      );
  }

  onClose(): void {
    this.contentEl.empty();
  }
}
