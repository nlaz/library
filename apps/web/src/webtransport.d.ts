// Minimal WebTransport declarations (not yet in TypeScript's DOM lib).
interface WebTransportHash {
  algorithm: "sha-256";
  value: BufferSource;
}

interface WebTransportOptions {
  serverCertificateHashes?: WebTransportHash[];
}

interface WebTransportDatagramDuplexStream {
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
}

declare class WebTransport {
  constructor(url: string, options?: WebTransportOptions);
  readonly ready: Promise<void>;
  readonly closed: Promise<unknown>;
  readonly datagrams: WebTransportDatagramDuplexStream;
  readonly incomingUnidirectionalStreams: ReadableStream<ReadableStream<Uint8Array>>;
  close(info?: { closeCode?: number; reason?: string }): void;
}
