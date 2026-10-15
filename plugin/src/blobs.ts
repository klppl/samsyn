import { requestUrl } from "obsidian";

/** Content-addressed blob transfer. `urlOf` builds the route (vault or guest share). */
export class BlobClient {
  constructor(
    private urlOf: (hash: string) => string,
    private bearer: string,
  ) {}

  static forVault(httpBase: string, vaultId: string, token: string): BlobClient {
    return new BlobClient((hash) => `${httpBase}/blobs/${encodeURIComponent(vaultId)}/${hash}`, token);
  }

  static forShare(httpBase: string, shareId: string, secret: string): BlobClient {
    return new BlobClient((hash) => `${httpBase}/guest/blobs/${encodeURIComponent(shareId)}/${hash}`, secret);
  }

  async upload(hash: string, data: ArrayBuffer): Promise<void> {
    const res = await requestUrl({
      url: this.urlOf(hash),
      method: "PUT",
      headers: { Authorization: `Bearer ${this.bearer}`, "Content-Type": "application/octet-stream" },
      body: data,
      throw: false,
    });
    if (res.status !== 201 && res.status !== 200) {
      throw new Error(`blob upload failed: HTTP ${res.status}`);
    }
  }

  async download(hash: string): Promise<ArrayBuffer | null> {
    const res = await requestUrl({
      url: this.urlOf(hash),
      method: "GET",
      headers: { Authorization: `Bearer ${this.bearer}` },
      throw: false,
    });
    if (res.status === 404) return null;
    if (res.status !== 200) throw new Error(`blob download failed: HTTP ${res.status}`);
    return res.arrayBuffer;
  }
}
