/**
 * Claude Code in a terminal tab: what the IDE decides about one without a
 * workbench (design §12.1, "Claude Code in a terminal tab").
 *
 * The server runs the program on a pseudo-terminal behind `/ide/terminal/<store>`
 * (`sc-server`'s `terminal.rs`). This module is the browser's end of that socket
 * — the protocol, the flow control, what a close means — written against a
 * minimal socket so it can be tested without one. `claudeTerminal.ts` hands it to
 * VS Code's terminal.
 */

import type { Origin } from "./languageServer";

/**
 * The socket URL for a terminal on `store`, started in `app`'s source directory
 * when one is given.
 *
 * Same origin and scheme rules as the language server's socket
 * (`languageServerUrl`): the session cookie authenticates it, and an `https:`
 * page needs `wss:`.
 */
export function terminalUrl(
  origin: Origin,
  store: string,
  app: string | null
): string {
  const scheme = origin.protocol === "https:" ? "wss:" : "ws:";
  const query = app == null ? "" : `?app=${encodeURIComponent(app)}`;
  return `${scheme}//${origin.host}/ide/terminal/${encodeURIComponent(
    store
  )}${query}`;
}

/** What a terminal tab is called: the application it works on, if any. */
export function terminalName(app: string | null): string {
  return app == null ? "Claude" : `Claude · ${app}`;
}

/** A request, carried by the IDE's URL, to open Claude Code once the workbench is up. */
export interface ClaudeRequest {
  /** The application to start in, or `null` for the store's root. */
  readonly app: string | null;
}

/**
 * The `claude=1&app=<name>` request on the page's URL, if there is one.
 *
 * This is what "Build with Claude" in the admin UI links to (TODO CT4).
 */
export function claudeRequest(href: string): ClaudeRequest | null {
  const params = new URL(href).searchParams;
  if (params.get("claude") !== "1") return null;
  const app = params.get("app")?.trim();
  return { app: app == null || app === "" ? null : app };
}

/**
 * The page's URL without the request, for `history.replaceState`.
 *
 * Removed once honoured, so that a reload — which keeps the URL — does not open
 * a second session on top of the one the workbench already shows.
 */
export function withoutClaudeRequest(href: string): string {
  const url = new URL(href);
  url.searchParams.delete("claude");
  url.searchParams.delete("app");
  return `${url.pathname}${url.search}${url.hash}`;
}

/**
 * How far the program's output may run ahead of the terminal drawing it, in
 * characters, before the server is asked to stop reading; and how far behind it
 * must fall again before it is asked to go on. VS Code's own numbers for its
 * own pseudo-terminals (`FlowControlConstants`).
 */
export const HIGH_WATERMARK_CHARS = 100_000;
export const LOW_WATERMARK_CHARS = 5_000;

/**
 * Output sent to the terminal and not yet drawn, and whether the server has
 * been asked to pause because of it.
 *
 * VS Code acknowledges drawn output in chunks of 5,000 characters and keeps the
 * remainder until there is a full chunk, so once everything is drawn up to
 * 5,000 can still be unacknowledged. Resuming *at* the low mark, not below it,
 * is what keeps that remainder from holding a paused terminal paused forever.
 */
export class FlowControl {
  private unacknowledged = 0;
  private paused = false;

  /** `chars` were handed to the terminal; `true` when the server must now pause. */
  sent(chars: number): boolean {
    this.unacknowledged += chars;
    if (!this.paused && this.unacknowledged > HIGH_WATERMARK_CHARS) {
      this.paused = true;
      return true;
    }
    return false;
  }

  /** The terminal drew `chars`; `true` when the server may now go on. */
  acknowledged(chars: number): boolean {
    this.unacknowledged = Math.max(0, this.unacknowledged - chars);
    if (this.paused && this.unacknowledged <= LOW_WATERMARK_CHARS) {
      this.paused = false;
      return true;
    }
    return false;
  }
}

/** What a closed socket means for the terminal showing it. */
export interface CloseOutcome {
  /** A line to print before the terminal stops, or `null` for none. */
  readonly message: string | null;
  /** The program's exit status, when it exited rather than was refused or lost. */
  readonly exitCode: number | undefined;
}

const EXITED = /^the program exited with status (\d+)$/;

/**
 * What a close frame says happened.
 *
 * - The program exited: its status, which VS Code reports itself (for a
 *   non-zero one), so nothing is printed.
 * - Any other reason — a refusal ("turned off", "not installed", "maximum of
 *   4 terminals"), a signal — is the server's own sentence, printed as sent.
 * - No reason at all is a connection that went away (a restart, a proxy, the
 *   network), which gets a sentence of its own.
 */
export function closeOutcome(reason: string): CloseOutcome {
  const given = reason.trim();
  const exited = EXITED.exec(given);
  if (exited != null) return { message: null, exitCode: Number(exited[1]) };
  return {
    message: given === "" ? "The connection to the server closed." : given,
    exitCode: undefined,
  };
}

/** The part of a `WebSocket` the connection uses. */
export interface TerminalSocket {
  binaryType: BinaryType;
  onopen: ((event: Event) => void) | null;
  onmessage: ((event: MessageEvent) => void) | null;
  onclose: ((event: CloseEvent) => void) | null;
  send(data: string | ArrayBufferView): void;
  close(code?: number, reason?: string): void;
}

/** Where a connection delivers what happens on it. */
export interface TerminalEvents {
  /** Output to draw. */
  data(text: string): void;
  /** The session is over; `code` as in {@link CloseOutcome}. */
  exit(code: number | undefined): void;
}

/** A terminal size in character cells. */
export interface TerminalSize {
  readonly cols: number;
  readonly rows: number;
}

/**
 * One terminal session over one socket.
 *
 * Binary frames are the terminal's bytes both ways; text frames carry the
 * control messages (`{"resize": …}`, `{"pause": …}`), which is the protocol
 * `terminal.rs` documents.
 */
export class TerminalConnection {
  /** Settles once the socket is open, or once it closed without opening. */
  readonly started: Promise<void>;

  private readonly decoder = new TextDecoder();
  private readonly encoder = new TextEncoder();
  private readonly flow = new FlowControl();
  /** Typed before the socket opened, sent once it has. */
  private pending: Uint8Array[] = [];
  private open = false;
  /** Closed, by either end: nothing typed from here on goes anywhere. */
  private ended = false;
  private closedByUs = false;

  constructor(
    private readonly socket: TerminalSocket,
    private size: TerminalSize,
    private readonly events: TerminalEvents
  ) {
    let started!: () => void;
    this.started = new Promise((resolve) => (started = resolve));
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      this.open = true;
      // The server starts the program at a default size; the real one is
      // sent first, so the first screen is drawn to fit.
      this.control({ resize: this.size });
      for (const bytes of this.pending) socket.send(bytes);
      this.pending = [];
      started();
    };
    socket.onmessage = (event) => {
      // The server sends no text frames; anything else is not output.
      if (!(event.data instanceof ArrayBuffer)) return;
      this.output(
        this.decoder.decode(new Uint8Array(event.data), { stream: true })
      );
    };
    socket.onclose = (event) => {
      this.open = false;
      this.ended = true;
      started();
      this.output(this.decoder.decode());
      if (this.closedByUs) {
        events.exit(undefined);
        return;
      }
      const outcome = closeOutcome(event.reason);
      if (outcome.message != null) events.data(`\r\n${outcome.message}\r\n`);
      events.exit(outcome.exitCode);
    };
  }

  /** What the admin typed. */
  input(text: string): void {
    this.write(this.encoder.encode(text));
  }

  /**
   * Input that is bytes rather than text — xterm's mouse reports in the
   * legacy encoding — one byte per character.
   */
  binary(data: string): void {
    this.write(Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff));
  }

  /** The terminal's new size. Sent once the socket is open if it is not yet. */
  resize(size: TerminalSize): void {
    this.size = size;
    if (this.open) this.control({ resize: size });
  }

  /** The terminal has drawn `chars` more characters of output. */
  acknowledge(chars: number): void {
    if (this.flow.acknowledged(chars)) this.control({ pause: false });
  }

  /** The tab was closed: end the session. The server ends the program. */
  shutdown(): void {
    this.closedByUs = true;
    this.ended = true;
    this.socket.close(1000);
  }

  private output(text: string): void {
    if (text === "") return;
    if (this.flow.sent(text.length)) this.control({ pause: true });
    this.events.data(text);
  }

  private write(bytes: Uint8Array): void {
    if (this.open) this.socket.send(bytes);
    else if (!this.ended) this.pending.push(bytes);
  }

  private control(
    message: { resize: TerminalSize } | { pause: boolean }
  ): void {
    this.socket.send(JSON.stringify(message));
  }
}
