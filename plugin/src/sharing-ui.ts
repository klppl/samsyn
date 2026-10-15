import { Notice, Setting } from "obsidian";
import { ReplicaApi, SharingInfo } from "./api";
import type ReplicaPlugin from "./main";
import { ConfirmModal, MountModal, NewTicketModal, ShareModal } from "./sharing-modals";

const accessText = (a: string) => (a === "ro" ? "read only" : "read & write");

function describeStatus(status: string): string {
  if (status === "active") return "live, same server";
  if (status === "remote: connected") return "live over the server link";
  if (status.startsWith("remote: connecting") || status.startsWith("remote: starting")) return "connecting to the owner's server…";
  if (status.startsWith("remote: reconnecting")) return `owner's server unreachable, retrying (${status.slice("remote: reconnecting".length).trim()})`;
  if (status.startsWith("remote: offline")) return "offline: p2p is disabled on this server";
  return status;
}

/**
 * The Sharing tab: what I share (with its tickets) and what others share with me. Every action
 * that produces a ticket or needs confirmation opens a modal, so the list can refresh underneath
 * without losing what the user is looking at.
 */
export class SharingSection {
  private api: ReplicaApi;
  private info: SharingInfo | null = null;
  private error: string | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor(
    private plugin: ReplicaPlugin,
    private root: HTMLElement,
  ) {
    this.api = new ReplicaApi(plugin.settings);
  }

  /** Poll while the tab is open so link status and revocations show up on their own. */
  start(): void {
    this.stop();
    this.timer = setInterval(() => void this.load(true), 10_000);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  private hasServer(): boolean {
    return !!(this.plugin.settings.serverUrl && this.plugin.settings.token);
  }

  async load(quiet = false): Promise<void> {
    if (!this.hasServer()) {
      this.render();
      return;
    }
    const before = this.info;
    try {
      const next = await this.api.sharing();
      this.error = null;
      if (before) this.announceChanges(before, next);
      if (quiet && JSON.stringify(before) === JSON.stringify(next)) return;
      this.info = next;
    } catch (e) {
      if (quiet) return;
      this.info = null;
      this.error = (e as Error).message;
    }
    this.render();
  }

  private announceChanges(before: SharingInfo, after: SharingInfo): void {
    for (const m of after.mounts) {
      const old = before.mounts.find((x) => x.id === m.id);
      if (old && old.status !== m.status && m.status.startsWith("revoked")) {
        new Notice(`Replica: access to "${m.name}" (${m.mount_path}) was revoked by the owner. Your copy stays as ordinary notes.`, 10000);
      }
    }
    for (const s of after.shares) {
      const old = before.shares.find((x) => x.id === s.id);
      if (old && s.mounted_by > old.mounted_by) new Notice(`Replica: someone mounted "${s.name}"`);
    }
  }

  private confirm(title: string, body: string, button: string, fn: () => Promise<void>): void {
    new ConfirmModal(this.plugin.app, title, body, button, async () => {
      await fn();
      await this.load();
    }).open();
  }

  render(): void {
    const el = this.root;
    el.empty();
    const reload = () => void this.load();

    if (!this.hasServer()) {
      this.renderGuestMounts(el, reload);
      return;
    }
    if (this.error) {
      el.createDiv({ cls: "replica-diag replica-error", text: `Cannot reach the sharing API: ${this.error}` });
      new Setting(el).addButton((b) => b.setButtonText("Retry").onClick(() => void this.load()));
      this.renderGuestMounts(el, reload);
      return;
    }
    if (!this.info) {
      el.createDiv({ cls: "replica-diag", text: "Loading…" });
      return;
    }
    const info = this.info;

    // ---------------------------------------------------------- shared by me
    new Setting(el)
      .setName("Shared by me")
      .setHeading()
      .setDesc("Folders or notes from this vault that other people edit live with you. Each person gets their own ticket.")
      .addButton((b) =>
        b
          .setButtonText("Share a folder or note…")
          .setCta()
          .onClick(() => new ShareModal(this.plugin, "", reload).open()),
      );

    if (info.shares.length === 0) {
      el.createDiv({ cls: "replica-empty", text: "Nothing shared yet. Right-click a folder or note and choose “Replica: Share…”, or use the button above." });
    }
    for (const s of info.shares) {
      const card = el.createDiv({ cls: "replica-card" });
      const active = s.grants.filter((g) => !g.revoked_at);
      new Setting(card)
        .setName(s.name)
        .setDesc(`${s.path} · ${active.length} active ticket${active.length === 1 ? "" : "s"} · mounted by ${s.mounted_by}`)
        .setClass("replica-card-head")
        .addButton((b) => b.setButtonText("New ticket…").onClick(() => new NewTicketModal(this.plugin, s, reload).open()))
        .addButton((b) =>
          b
            .setButtonText("Unshare")
            .setWarning()
            .onClick(() =>
              this.confirm(
                `Stop sharing ${s.name}?`,
                `Everyone who mounted ${s.path} stops receiving changes. They keep their copy as ordinary notes; your files are untouched.`,
                "Unshare",
                () => this.api.unshare(s.id),
              ),
            ),
        );
      for (const g of s.grants) {
        const uses = g.mounted_by ? ` · mounted by ${g.mounted_by}` : "";
        const row = new Setting(card)
          .setClass("replica-grant")
          .setName(`ticket “${g.label}”`)
          .setDesc(g.revoked_at ? `${accessText(g.access)} · revoked ${g.revoked_at.slice(0, 10)}` : `${accessText(g.access)} · issued ${g.created_at.slice(0, 10)}${uses}`);
        if (g.revoked_at) row.setClass("replica-revoked");
        else {
          row.addButton((b) =>
            b.setButtonText("Revoke").onClick(() =>
              this.confirm(
                `Revoke ticket “${g.label}”?`,
                `Anyone who mounted ${s.name} with this ticket stops receiving changes and can no longer push edits. Other tickets keep working.`,
                "Revoke",
                () => this.api.revoke(s.id, g.label),
              ),
            ),
          );
        }
      }
    }

    // ---------------------------------------------------------- shared with me
    new Setting(el)
      .setName("Shared with me")
      .setHeading()
      .setDesc("Folders or notes other people share with you. They live in a folder of your choice and sync live.")
      .addButton((b) =>
        b
          .setButtonText("Add from ticket…")
          .setCta()
          .onClick(() => new MountModal(this.plugin, reload).open()),
      );
    if (info.mounts.length === 0 && this.plugin.settings.guestMounts.length === 0) {
      el.createDiv({ cls: "replica-empty", text: "Nothing mounted. Paste a ticket someone sent you with the button above." });
    }
    this.renderGuestCards(el);
    for (const m of info.mounts) {
      const revoked = m.status.startsWith("revoked");
      const card = el.createDiv({ cls: "replica-card" + (revoked ? " replica-revoked" : "") });
      const row = new Setting(card).setName(m.name).setClass("replica-card-head");
      if (revoked) row.setDesc(`${m.mount_path} · access revoked by the owner · no longer syncing · your copy stays as ordinary notes`);
      else row.setDesc(`${m.mount_path} · ${accessText(m.access)} · ${describeStatus(m.status)}`);
      row.addButton((b) =>
        b.setButtonText(revoked ? "Remove" : "Unmount").onClick(() =>
          this.confirm(
            revoked ? `Remove ${m.name} from the list?` : `Unmount ${m.name}?`,
            `The files under ${m.mount_path} stay in your vault as ordinary notes; they just stop syncing with the owner.`,
            revoked ? "Remove" : "Unmount",
            () => this.api.unmount(m.id),
          ),
        ),
      );
    }

    el.createDiv({ cls: "replica-diag replica-muted", text: `This server's node id: ${info.node_id.slice(0, 16)}…` });
  }

  /** "Shared with me" when there is no server of our own: guest mounts only. */
  private renderGuestMounts(el: HTMLElement, reload: () => void): void {
    new Setting(el)
      .setName("Shared with me")
      .setHeading()
      .setDesc("Folders or notes other people share with you. This device talks to their server directly.")
      .addButton((b) =>
        b
          .setButtonText("Add from ticket…")
          .setCta()
          .onClick(() => new MountModal(this.plugin, reload).open()),
      );
    if (this.plugin.settings.guestMounts.length === 0) {
      el.createDiv({ cls: "replica-empty", text: "Nothing mounted. Paste a ticket someone sent you with the button above." });
    }
    this.renderGuestCards(el);
  }

  private renderGuestCards(el: HTMLElement): void {
    for (const m of this.plugin.settings.guestMounts) {
      const d = this.plugin.guestEngine(m.id)?.getDiagnostics();
      const status = !m.enabled
        ? "paused"
        : !d
          ? "starting…"
          : d.status === "connected"
            ? `live as guest · ${d.peers} other${d.peers === 1 ? "" : "s"} online`
            : d.status === "connecting"
              ? "connecting to the owner's server…"
              : `offline${d.lastError ? ` · ${d.lastError}` : ""}`;
      const card = el.createDiv({ cls: "replica-card" });
      new Setting(card)
        .setName(m.name)
        .setClass("replica-card-head")
        .setDesc(`${m.mountPath} · ${accessText(m.access)} · guest of ${m.url.replace(/^https?:\/\//, "")} · ${status}`)
        .addButton((b) =>
          b.setButtonText("Unmount").onClick(() =>
            this.confirm(
              `Unmount ${m.name}?`,
              `The files under ${m.mountPath} stay in your vault as ordinary notes; they just stop syncing with the owner.`,
              "Unmount",
              () => this.plugin.removeGuestMount(m.id),
            ),
          ),
        );
    }
  }
}
