/**
 * The browser's end of the terminal socket (design §12.1): URLs, the
 * `claude=1` request, flow control, and a session against a fake socket.
 */

import { describe, expect, it } from "vitest";

import {
  FlowControl,
  HIGH_WATERMARK_CHARS,
  LOW_WATERMARK_CHARS,
  TerminalConnection,
  claudeRequest,
  closeOutcome,
  terminalName,
  terminalUrl,
  withoutClaudeRequest,
  type TerminalSocket,
} from "./terminal";

describe("where a terminal's socket is", () => {
  it("is the same origin as the page, on the route the server mounts", () => {
    expect(
      terminalUrl(
        { protocol: "http:", host: "localhost:3032" },
        "app-source",
        null
      )
    ).toBe("ws://localhost:3032/ide/terminal/app-source");
  });

  it("names the application to start in, and uses wss: under TLS", () => {
    expect(
      terminalUrl({ protocol: "https:", host: "h" }, "my store", "Shop & Co")
    ).toBe("wss://h/ide/terminal/my%20store?app=Shop%20%26%20Co");
  });

  it("calls the tab after the application", () => {
    expect(terminalName("Shop")).toBe("Claude · Shop");
    expect(terminalName(null)).toBe("Claude");
  });
});

describe("the request to open Claude Code on the IDE's URL", () => {
  it("is read from claude=1, with the application if there is one", () => {
    expect(claudeRequest("http://h/ide/?store=s&claude=1&app=Shop")).toEqual({
      app: "Shop",
    });
    expect(claudeRequest("http://h/ide/?store=s&claude=1")).toEqual({
      app: null,
    });
    expect(claudeRequest("http://h/ide/?store=s&claude=1&app=%20")).toEqual({
      app: null,
    });
  });

  it("is not there without claude=1", () => {
    expect(claudeRequest("http://h/ide/?store=s&app=Shop")).toBeNull();
    expect(claudeRequest("http://h/ide/?store=s&claude=0")).toBeNull();
  });

  /** A reload keeps the URL; without this it would open a second session. */
  it("is removed once honoured, keeping everything else", () => {
    expect(
      withoutClaudeRequest(
        "http://h/ide/?store=s&claude=1&app=Shop&path=src%2Fa.ts#x"
      )
    ).toBe("/ide/?store=s&path=src%2Fa.ts#x");
  });
});

describe("what a closed socket means", () => {
  it("is an exit status when the program exited, printed by VS Code rather than here", () => {
    expect(closeOutcome("the program exited with status 3")).toEqual({
      message: null,
      exitCode: 3,
    });
  });

  it("is the server's own sentence for a refusal", () => {
    expect(
      closeOutcome("the IDE terminal is turned off (Settings → Development)")
    ).toEqual({
      message: "the IDE terminal is turned off (Settings → Development)",
      exitCode: undefined,
    });
  });

  it("says something when the connection went away without a reason", () => {
    expect(closeOutcome("").message).toBe(
      "The connection to the server closed."
    );
  });
});

describe("flow control", () => {
  it("pauses once, past the high mark, and resumes once, at the low mark", () => {
    const flow = new FlowControl();
    expect(flow.sent(HIGH_WATERMARK_CHARS)).toBe(false);
    expect(flow.sent(1)).toBe(true);
    expect(flow.sent(50_000)).toBe(false);
    expect(flow.acknowledged(100_000)).toBe(false);
    expect(flow.acknowledged(150_001 - 100_000 - LOW_WATERMARK_CHARS)).toBe(
      true
    );
    expect(flow.acknowledged(5_000)).toBe(false);
  });

  /**
   * VS Code acknowledges in 5,000-character chunks and holds back the rest, so
   * a terminal that has drawn everything can still owe up to 5,000.
   */
  it("does not stay paused on VS Code's unacknowledged remainder", () => {
    const flow = new FlowControl();
    flow.sent(HIGH_WATERMARK_CHARS + LOW_WATERMARK_CHARS);
    expect(flow.acknowledged(HIGH_WATERMARK_CHARS)).toBe(true);
  });
});

/** A socket that records what is sent and lets the test play the server. */
class FakeSocket implements TerminalSocket {
  binaryType: BinaryType = "blob";
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  readonly sent: Array<string | Uint8Array> = [];
  closedWith: number | undefined;

  send(data: string | ArrayBufferView): void {
    this.sent.push(
      typeof data === "string"
        ? data
        : new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
    );
  }
  close(code?: number): void {
    this.closedWith = code;
  }

  serverOpens(): void {
    this.onopen?.(new Event("open"));
  }
  serverSends(bytes: Uint8Array | string): void {
    const data = typeof bytes === "string" ? bytes : bytes.slice().buffer;
    this.onmessage?.({ data } as MessageEvent);
  }
  serverCloses(code: number, reason: string): void {
    this.onclose?.({ code, reason } as CloseEvent);
  }
  controls(): unknown[] {
    return this.sent
      .filter((s) => typeof s === "string")
      .map((s) => JSON.parse(s as string));
  }
  typed(): string {
    return this.sent
      .filter((s): s is Uint8Array => typeof s !== "string")
      .map((b) => new TextDecoder().decode(b))
      .join("");
  }
}

function connect(size = { cols: 120, rows: 40 }) {
  const socket = new FakeSocket();
  const output: string[] = [];
  const exits: Array<number | undefined> = [];
  const connection = new TerminalConnection(socket, size, {
    data: (text) => output.push(text),
    exit: (code) => exits.push(code),
  });
  return { socket, connection, output, exits };
}

describe("a terminal session", () => {
  it("sends the real size first, then what was typed before the socket opened", async () => {
    const { socket, connection } = connect();
    expect(socket.binaryType).toBe("arraybuffer");
    connection.input("ls\r");
    expect(socket.sent).toEqual([]);
    socket.serverOpens();
    await connection.started;
    expect(socket.controls()).toEqual([{ resize: { cols: 120, rows: 40 } }]);
    expect(socket.typed()).toBe("ls\r");
  });

  it("sends input as bytes and a resize as a control message", () => {
    const { socket, connection } = connect();
    socket.serverOpens();
    connection.input("é\u0003");
    connection.resize({ cols: 80, rows: 24 });
    expect(socket.typed()).toBe("é\u0003");
    expect(socket.controls()).toEqual([
      { resize: { cols: 120, rows: 40 } },
      { resize: { cols: 80, rows: 24 } },
    ]);
  });

  it("sends a mouse report one byte per character", () => {
    const { socket, connection } = connect();
    socket.serverOpens();
    connection.binary("\x1b[M\xa0!!");
    const bytes = socket.sent.at(-1) as Uint8Array;
    expect(Array.from(bytes)).toEqual([0x1b, 0x5b, 0x4d, 0xa0, 0x21, 0x21]);
  });

  /** A frame may end in the middle of a character; the decoder must wait for the rest. */
  it("draws output, joining a character split across frames", () => {
    const { socket, output } = connect();
    socket.serverOpens();
    const bytes = new TextEncoder().encode("ok ✓");
    socket.serverSends(bytes.slice(0, 4));
    socket.serverSends(bytes.slice(4));
    expect(output.join("")).toBe("ok ✓");
  });

  it("ignores a text frame rather than drawing it", () => {
    const { socket, output } = connect();
    socket.serverOpens();
    socket.serverSends("not output");
    expect(output).toEqual([]);
  });

  it("pauses the server when output runs ahead, and resumes once it is drawn", () => {
    const { socket, connection } = connect();
    socket.serverOpens();
    socket.serverSends(
      new TextEncoder().encode("x".repeat(HIGH_WATERMARK_CHARS + 1))
    );
    expect(socket.controls().at(-1)).toEqual({ pause: true });
    connection.acknowledge(HIGH_WATERMARK_CHARS);
    expect(socket.controls().at(-1)).toEqual({ pause: false });
  });

  it("prints a refusal and ends without an exit status", () => {
    const { socket, output, exits } = connect();
    socket.serverOpens();
    socket.serverCloses(1008, "claude is not installed on this server");
    expect(output.join("")).toContain("claude is not installed on this server");
    expect(exits).toEqual([undefined]);
  });

  it("ends with the program's exit status", () => {
    const { socket, output, exits } = connect();
    socket.serverOpens();
    socket.serverCloses(1000, "the program exited with status 0");
    expect(output).toEqual([]);
    expect(exits).toEqual([0]);
  });

  it("closes the socket normally when the tab is closed, printing nothing", () => {
    const { socket, connection, output, exits } = connect();
    socket.serverOpens();
    connection.shutdown();
    expect(socket.closedWith).toBe(1000);
    socket.serverCloses(1000, "");
    expect(output).toEqual([]);
    expect(exits).toEqual([undefined]);
    connection.input("late");
    expect(socket.typed()).toBe("");
  });
});
