import { Notice, Setting } from "obsidian";
import { ReplicaApi, SharingInfo } from "./api";
import type ReplicaPlugin from "./main";

async function copy(text: string, what = "Ticket"): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    new Notice(`${what} copied to clipboard`);
  } catch {
    new Notice(`Could not access the clipboard; select and copy the ${what.toLowerCase()} manually`);
  }
}

function showTicket(container: HTMLElement, ticket: string, label: string): void {
  container.empty();
  const box = container.createDiv({ cls: "replica-ticket" });
  box.createDiv({ cls: "replica-ticket-label", text: `Ticket "${label}" (shown once; copy it now)` });
  const ta = box.createEl("textarea", { cls: "replica-ticket-text" });
  ta.value = ticket;
  ta.readOnly = true;
  ta.rows = 3;
  ta.onclick = () => ta.select();
  new Setting(box).addButton((b) => b.setButtonText("Copy ticket").setCta().onClick(() => void copy(ticket)));
}

/** Renders the Sharing section of the settings tab. Pure UI over the server API. */
export class SharingSection {
  private api: ReplicaApi;
  private info: SharingInfo | null = null;
  private error: string | null = null;

  constructor(private plugin: ReplicaPlugin, private root: HTMLElement) {
    this.api = new ReplicaApi(plugin.settings);
  }

  async load(): Promise<void> {
    this.error = null;
    try {
      this.info = await this.api.sharing();
    } catch (e) {
      this.info = null;
      this.error = (e as Error).message;
    }
    this.render();
  }

  private async act(label: string, fn: () => Promise<unknown>): Promise<void> {
    try {
      await fn();
      await this.load();
    } catch (e) {
      new Notice(`Replica: ${label} failed: ${(e as Error).message}`, 8000);
    }
  }

  render(): void {
    const el = this.root;
    el.empty();
    new Setting(el).setName("Sharing").setHeading();

    if (this.error) {
      el.createDiv({ cls: "replica-diag", text: `Cannot reach the sharing API: ${this.error}` });
      new Setting(el).addButton((b) => b.setButtonText("Retry").onClick(() => void this.load()));
      return;
    }
    if (!this.info) {
      el.createDiv({ cls: "replica-diag", text: "Loading…" });
      return;
    }
    const info = this.info;

    // ---------------------------------------------------------- shared by me
    new Setting(el).setName("Shared by me").setDesc("Folders or notes from this vault that others can edit live with you.");
    const ticketOut = el.createDiv();

    let path = this.plugin.pendingSharePath ?? "";
    this.plugin.pendingSharePath = null;
    let name = "";
    let access = "rw";
    const create = new Setting(el)
      .setName("Share a folder or note")
      .setDesc("Path inside the vault, e.g. Projects/X or Notes/plan.md. One ticket covers exactly this path.");
    create.addText((t) => {
      t.setPlaceholder("Projects/X").setValue(path).onChange((v) => (path = v));
      t.inputEl.addClass("replica-wide");
    });
    create.addText((t) => t.setPlaceholder("name (optional)").onChange((v) => (name = v)));
    create.addDropdown((d) => d.addOptions({ rw: "read & write", ro: "read only" }).setValue(access).onChange((v) => (access = v)));
    create.addButton((b) =>
      b
        .setButtonText("Create")
        .setCta()
        .onClick(() =>
          this.act("create share", async () => {
            if (!path.trim()) throw new Error("enter a path");
            const r = await this.api.createShare(path.trim(), name.trim(), access);
            showTicket(ticketOut, r.ticket, r.label);
            new Notice(`Replica: sharing ${r.share.path}`);
          }),
        ),
    );

    if (info.shares.length === 0) {
      el.createDiv({ cls: "replica-diag", text: "Nothing shared yet." });
    }
    for (const s of info.shares) {
      const active = s.grants.filter((g) => !g.revoked_at);
      const row = new Setting(el)
        .setName(`${s.name}  ·  ${s.path}`)
        .setDesc(`${active.length} active ticket${active.length === 1 ? "" : "s"}, mounted by ${s.mounted_by}`);
      row.addButton((b) =>
        b.setButtonText("New ticket").onClick(() =>
          this.act("new ticket", async () => {
            const r = await this.api.newTicket(s.id, "rw");
            showTicket(ticketOut, r.ticket, r.label);
          }),
        ),
      );
      row.addButton((b) =>
        b.setButtonText("New read-only ticket").onClick(() =>
          this.act("new ticket", async () => {
            const r = await this.api.newTicket(s.id, "ro");
            showTicket(ticketOut, r.ticket, r.label);
          }),
        ),
      );
      row.addButton((b) =>
        b
          .setButtonText("Unshare")
          .setWarning()
          .onClick(() =>
            this.act("unshare", async () => {
              await this.api.unshare(s.id);
              new Notice(`Replica: ${s.name} is no longer shared. Your files are untouched.`);
            }),
          ),
      );
      for (const g of s.grants) {
        const gr = new Setting(el)
          .setClass("replica-grant")
          .setName(`ticket "${g.label}" · ${g.access === "ro" ? "read only" : "read & write"}`)
          .setDesc(g.revoked_at ? `revoked ${g.revoked_at.slice(0, 10)}` : `issued ${g.created_at.slice(0, 10)}`);
        if (!g.revoked_at) {
          gr.addButton((b) =>
            b.setButtonText("Revoke").onClick(() =>
              this.act("revoke", async () => {
                await this.api.revoke(s.id, g.label);
                new Notice(`Replica: ticket "${g.label}" revoked`);
              }),
            ),
          );
        }
      }
    }

    // ---------------------------------------------------------- shared with me
    new Setting(el).setName("Shared with me").setDesc("Paste a ticket from someone else to get their folder or note in this vault.");
    let ticket = "";
    let mountPath = "";
    const add = new Setting(el).setName("Add from ticket").setDesc("Mount path defaults to Shared/<name>. It must be an empty or new folder.");
    add.addTextArea((t) => {
      t.setPlaceholder("replica…").onChange((v) => (ticket = v));
      t.inputEl.rows = 2;
      t.inputEl.addClass("replica-wide");
    });
    add.addText((t) => t.setPlaceholder("Shared/Name").onChange((v) => (mountPath = v)));
    add.addButton((b) =>
      b
        .setButtonText("Mount")
        .setCta()
        .onClick(() =>
          this.act("mount", async () => {
            if (!ticket.trim()) throw new Error("paste a ticket first");
            const r = await this.api.mount(ticket.trim(), mountPath.trim());
            new Notice(`Replica: mounted at ${r.mount_path} (${r.access === "ro" ? "read only" : "read & write"}). Files arrive shortly.`);
          }),
        ),
    );
    if (info.mounts.length === 0) {
      el.createDiv({ cls: "replica-diag", text: "Nothing mounted." });
    }
    for (const m of info.mounts) {
      const row = new Setting(el)
        .setName(`${m.name}  ·  ${m.mount_path}`)
        .setDesc(`${m.access === "ro" ? "read only" : "read & write"} · ${m.status}`);
      row.addButton((b) =>
        b.setButtonText("Unmount").onClick(() =>
          this.act("unmount", async () => {
            await this.api.unmount(m.id);
            new Notice(`Replica: unmounted ${m.mount_path}. The files stay in your vault as ordinary notes.`);
          }),
        ),
      );
    }

    el.createDiv({ cls: "replica-diag replica-muted", text: `This server's node id: ${info.node_id.slice(0, 16)}…` });
  }
}
