import { requestUrl } from "obsidian";
import { ReplicaSettings, serverUrls } from "./settings";

export interface Grant {
  label: string;
  access: string;
  created_at: string;
  revoked_at: string | null;
}
export interface ShareRow {
  id: string;
  name: string;
  path: string;
  prefix: string;
  only_file: string | null;
  grants: Grant[];
  mounted_by: number;
}
export interface MountRow {
  id: string;
  share_id: string;
  name: string;
  mount_path: string;
  access: string;
  status: string;
}
export interface SharingInfo {
  node_id: string;
  shares: ShareRow[];
  mounts: MountRow[];
}

/** Thin client for the server's sharing API. Everything else in the plugin is plain y-websocket. */
export class ReplicaApi {
  constructor(private settings: ReplicaSettings) {}

  private base(): string {
    const u = serverUrls(this.settings.serverUrl);
    if (!u) throw new Error("no server URL configured");
    return u.http;
  }

  private async call<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await requestUrl({
      url: `${this.base()}${path}`,
      method,
      headers: { Authorization: `Bearer ${this.settings.token}`, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      throw: false,
    });
    if (res.status === 204) return undefined as T;
    let data: any = null;
    try {
      data = res.json;
    } catch {
      /* non-json */
    }
    if (res.status >= 400) {
      throw new Error(data?.error ?? `HTTP ${res.status}`);
    }
    return data as T;
  }

  private vault(): string {
    return encodeURIComponent(this.settings.vaultId);
  }

  sharing(): Promise<SharingInfo> {
    return this.call("GET", `/api/vaults/${this.vault()}/sharing`);
  }
  createShare(path: string, name: string, access: string): Promise<{ share: ShareRow; ticket: string; label: string }> {
    return this.call("POST", `/api/vaults/${this.vault()}/shares`, { path, name: name || undefined, access });
  }
  newTicket(shareId: string, access: string, label?: string): Promise<{ ticket: string; label: string; access: string }> {
    return this.call("POST", `/api/shares/${shareId}/tickets`, { access, label: label || undefined });
  }
  revoke(shareId: string, label: string): Promise<void> {
    return this.call("DELETE", `/api/shares/${shareId}/grants/${encodeURIComponent(label)}`);
  }
  unshare(shareId: string): Promise<void> {
    return this.call("DELETE", `/api/shares/${shareId}`);
  }
  mount(ticket: string, mountPath: string): Promise<{ mount_path: string; access: string }> {
    return this.call("POST", `/api/vaults/${this.vault()}/mounts`, { ticket, mount_path: mountPath || undefined });
  }
  unmount(mountId: string): Promise<void> {
    return this.call("DELETE", `/api/mounts/${mountId}`);
  }
}
