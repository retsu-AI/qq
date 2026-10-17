/** Keeps a draft until the server accepts it, even if the composer changes sessions. */
export class Drafts {
  private values = new Map<string, string>();
  private pending = new Set<string>();

  get(key: string): string { return this.values.get(key) ?? ""; }
  set(key: string, text: string) {
    if (text) this.values.set(key, text);
    else this.values.delete(key);
  }

  async submit(key: string, send: (text: string) => Promise<void>) {
    if (this.pending.has(key)) throw new Error("This draft is awaiting acknowledgement");
    const original = this.get(key);
    if (!original.trim()) throw new Error("Write a message first");
    this.pending.add(key);
    try {
      await send(original.trim());
      if (this.get(key) === original) this.values.delete(key);
    } finally {
      this.pending.delete(key);
    }
  }
}

export const drafts = new Drafts();
