import { AbstractInputSuggest, App, Modal, Notice, Setting, TAbstractFile, TFolder } from "obsidian";
import { ReplicaApi, ShareRow, TicketInfo } from "./api";
import type ReplicaPlugin from "./main";

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

const accessText = (a: string) => (a === "ro" ? "read only" : "read & write");

/** Shows a freshly issued ticket. Tickets are shown once; the server only stores a hash. */
function renderTicket(el: HTMLElement, ticket: string, title: string, hint: string): void {
  el.empty();
  el.createEl("h3", { text: title });
  el.createEl("p", { text: hint, cls: "replica-muted" });
  const ta = el.createEl("textarea", { cls: "replica-ticket-text" });
  ta.value = ticket;
  ta.readOnly = true;
  ta.rows = 4;
  ta.onclick = () => ta.select();
  new Setting(el)
    .addButton((b) => b.setButtonText("Copy ticket").setCta().onClick(() => void copyText(ticket)))
    .addButton((b) => b.setButtonText("Done").onClick(() => el.closest(".modal-container")?.querySelector<HTMLElement>(".modal-close-button")?.click()));
}

/** Share a folder or a note: pick the path, get the first ticket. */
export class ShareModal extends Modal {
  private api: ReplicaApi;
  private path: string;
  private name = "";
  private access = "rw";

  constructor(
    private plugin: ReplicaPlugin,
    initialPath = "",
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.api = new ReplicaApi(plugin.settings);
    this.path = initialPath;
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("replica-modal");
    contentEl.createEl("h2", { text: "Share a folder or note" });
    contentEl.createEl("p", {
      cls: "replica-muted",
      text: "Whoever gets the ticket sees exactly this path, live, in their own vault. Nothing else in your vault is visible to them. One ticket covers one path; share another path with another ticket.",
    });
    new Setting(contentEl)
      .setName("Folder or note")
      .setDesc("A path in this vault, e.g. Projects/X or Notes/plan.md")
      .addText((t) => {
        t.setPlaceholder("Projects/X").setValue(this.path).onChange((v) => (this.path = v));
        t.inputEl.addClass("replica-wide");
        new PathSuggest(this.app, t.inputEl);
      });
    new Setting(contentEl)
      .setName("Name")
      .setDesc("How the other person sees it. Defaults to the folder or note name.")
      .addText((t) => t.setPlaceholder("optional").onChange((v) => (this.name = v)));
    new Setting(contentEl)
      .setName("Access for the first ticket")
      .setDesc("Read-only people see your edits but cannot change anything. You can issue more tickets later.")
      .addDropdown((d) => d.addOptions({ rw: "Read & write", ro: "Read only" }).setValue(this.access).onChange((v) => (this.access = v)));
    const msg = contentEl.createDiv({ cls: "replica-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Create share")
        .setCta()
        .onClick(async () => {
          const p = this.path.trim().replace(/^\/+|\/+$/g, "");
          if (!p) {
            msg.setText("Enter a path.");
            return;
          }
          b.setDisabled(true);
          try {
            const r = await this.api.createShare(p, this.name.trim(), this.access);
            this.onDone?.();
            renderTicket(
              contentEl,
              r.ticket,
              `Sharing ${r.share.path}`,
              `Send this ticket (${accessText(this.access)}) to the person you want to work with. It is shown once; you can always create a new one.`,
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
  private api: ReplicaApi;
  private label = "";
  private access = "rw";

  constructor(
    private plugin: ReplicaPlugin,
    private share: ShareRow,
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.api = new ReplicaApi(plugin.settings);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("replica-modal");
    contentEl.createEl("h2", { text: `New ticket for ${this.share.name}` });
    contentEl.createEl("p", { cls: "replica-muted", text: `${this.share.path} · each person should get their own ticket, so you can revoke one without affecting the others.` });
    new Setting(contentEl)
      .setName("Label")
      .setDesc("Who this ticket is for, e.g. the person's name.")
      .addText((t) => t.setPlaceholder("anna").onChange((v) => (this.label = v)));
    new Setting(contentEl)
      .setName("Access")
      .addDropdown((d) => d.addOptions({ rw: "Read & write", ro: "Read only" }).setValue(this.access).onChange((v) => (this.access = v)));
    const msg = contentEl.createDiv({ cls: "replica-error" });
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText("Create ticket")
        .setCta()
        .onClick(async () => {
          b.setDisabled(true);
          try {
            const r = await this.api.newTicket(this.share.id, this.access, this.label.trim());
            this.onDone?.();
            renderTicket(contentEl, r.ticket, `Ticket "${r.label}"`, `${accessText(r.access)} access to ${this.share.path}. Shown once.`);
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

/** Paste a ticket, see what it is, choose where it goes, mount. */
export class MountModal extends Modal {
  private api: ReplicaApi;
  private ticket = "";
  private mountPath = "";
  private info: TicketInfo | null = null;
  private inspectTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private plugin: ReplicaPlugin,
    private onDone?: () => void,
  ) {
    super(plugin.app);
    this.api = new ReplicaApi(plugin.settings);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.addClass("replica-modal");
    contentEl.createEl("h2", { text: "Add a shared folder or note" });
    contentEl.createEl("p", { cls: "replica-muted", text: "Paste the ticket you received. The shared files appear in a folder of your choice and stay live in both directions." });
    const ta = contentEl.createEl("textarea", { cls: "replica-ticket-text" });
    ta.placeholder = "replica…";
    ta.rows = 3;
    const card = contentEl.createDiv({ cls: "replica-ticket-info" });
    const pathSetting = new Setting(contentEl).setName("Put it in").setDesc("A new or empty folder in this vault.");
    let pathInput: HTMLInputElement | null = null;
    pathSetting.addText((t) => {
      t.setPlaceholder("Shared/Name").onChange((v) => (this.mountPath = v));
      pathInput = t.inputEl;
    });
    const msg = contentEl.createDiv({ cls: "replica-error" });
    let mountBtn: import("obsidian").ButtonComponent | null = null;
    new Setting(contentEl).addButton((b) => {
      mountBtn = b;
      b.setButtonText("Mount")
        .setCta()
        .setDisabled(true)
        .onClick(async () => {
          b.setDisabled(true);
          try {
            const r = await this.api.mount(this.ticket.trim(), this.mountPath.trim());
            new Notice(`Replica: mounted at ${r.mount_path} (${accessText(r.access)}). Files arrive shortly.`, 8000);
            this.onDone?.();
            this.close();
          } catch (e) {
            msg.setText((e as Error).message);
            b.setDisabled(false);
          }
        });
    });

    const inspect = async () => {
      const t = this.ticket.trim();
      card.empty();
      msg.setText("");
      this.info = null;
      mountBtn?.setDisabled(true);
      if (!t) return;
      try {
        const info = await this.api.inspectTicket(t);
        if (this.ticket.trim() !== t) return; // superseded
        this.info = info;
        const row = (label: string, value: string, cls = "") => {
          const r = card.createDiv({ cls: "replica-diag-row" });
          r.createSpan({ cls: "replica-diag-label", text: label });
          r.createSpan({ cls: "replica-diag-value " + cls, text: value });
        };
        row("Share", info.name);
        row("Access", accessText(info.access));
        row("Where", info.same_server ? "this server" : `another server (${info.node_id.slice(0, 10)}…), linked peer-to-peer`);
        if (info.from_own_vault) row("Owner", `you (vault "${info.from_own_vault}")`, "is-warn");
        if (info.valid === false) {
          row("Status", "invalid or revoked", "is-bad");
          return;
        }
        if (!info.same_server && !info.p2p_available) {
          row("Status", "this server runs with p2p disabled; cannot mount tickets from other servers", "is-bad");
          return;
        }
        if (!this.mountPath.trim() && pathInput) {
          this.mountPath = `Shared/${info.name}`;
          pathInput.value = this.mountPath;
        }
        mountBtn?.setDisabled(false);
      } catch (e) {
        msg.setText((e as Error).message);
      }
    };
    ta.addEventListener("input", () => {
      this.ticket = ta.value;
      if (this.inspectTimer) clearTimeout(this.inspectTimer);
      this.inspectTimer = setTimeout(() => void inspect(), 300);
    });
    ta.focus();
  }

  onClose(): void {
    if (this.inspectTimer) clearTimeout(this.inspectTimer);
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
    contentEl.addClass("replica-modal");
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
              new Notice(`Replica: ${(e as Error).message}`, 8000);
              b.setDisabled(false);
            }
          }),
      );
  }

  onClose(): void {
    this.contentEl.empty();
  }
}
