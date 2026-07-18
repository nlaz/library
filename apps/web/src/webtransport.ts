// WebTransport transport: datagrams out (keystrokes are disposable), one uni
// stream per response so a slow payload never blocks a fresh one.

import type { Transport } from "./transport";
import type { Collections, QueryMsg, WireResponse } from "./types";

const WT_URL = "https://127.0.0.1:4433/";

export class WtTransport implements Transport {
  private wt!: WebTransport;
  private writer!: WritableStreamDefaultWriter<Uint8Array>;
  private cb: (msg: WireResponse) => void = () => {};
  private enc = new TextEncoder();

  async ready(): Promise<void> {
    const hash: number[] = await (await fetch("/api/cert_hash")).json();
    this.wt = new WebTransport(WT_URL, {
      serverCertificateHashes: [
        { algorithm: "sha-256", value: new Uint8Array(hash) },
      ],
    });
    await this.wt.ready;
    this.writer = this.wt.datagrams.writable.getWriter();
    this.readResponses();
  }

  send(q: QueryMsg): void {
    this.writer.write(this.enc.encode(JSON.stringify(q)));
  }

  onResponse(cb: (msg: WireResponse) => void): void {
    this.cb = cb;
  }

  async complete(prefix: string): Promise<string[]> {
    return (await fetch(`/api/complete?q=${encodeURIComponent(prefix)}`)).json();
  }

  async collections(): Promise<Collections> {
    return (await fetch("/api/collections")).json();
  }

  private readResponses() {
    (async () => {
      const streams = this.wt.incomingUnidirectionalStreams.getReader();
      for (;;) {
        const { value, done } = await streams.read();
        if (done) break;
        this.readOne(value);
      }
    })();
  }

  private async readOne(stream: ReadableStream<Uint8Array>) {
    const buf = await new Response(stream).arrayBuffer();
    this.cb(JSON.parse(new TextDecoder().decode(buf)));
  }
}
