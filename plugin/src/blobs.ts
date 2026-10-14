import { requestUrl } from "obsidian";

export class BlobClient {
  constructor(private httpBase: string, private vaultId: string, private token: string) {}

  private url(hash: string): string {
    return `${this.httpBase}/blobs/${encodeURIComponent(this.vaultId)}/${hash}`;
  }

  async upload(hash: string, data: ArrayBuffer): Promise<void> {
    const res = await requestUrl({
      url: this.url(hash),
      method: "PUT",
      headers: { Authorization: `Bearer ${this.token}`, "Content-Type": "application/octet-stream" },
      body: data,
      throw: false,
    });
    if (res.status !== 201 && res.status !== 200) {
      throw new Error(`blob upload failed: HTTP ${res.status}`);
    }
  }

  async download(hash: string): Promise<ArrayBuffer | null> {
    const res = await requestUrl({
      url: this.url(hash),
      method: "GET",
      headers: { Authorization: `Bearer ${this.token}` },
      throw: false,
    });
    if (res.status === 404) return null;
    if (res.status !== 200) throw new Error(`blob download failed: HTTP ${res.status}`);
    return res.arrayBuffer;
  }
}
