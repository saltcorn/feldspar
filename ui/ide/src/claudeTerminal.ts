/**
 * Claude Code in a terminal tab: the VS Code half (design §12.1, "Claude Code in
 * a terminal tab").
 *
 * The terminal is VS Code's own — xterm, tabs, the panel, find, links — over a
 * terminal *process* of ours, whose bytes go to `/ide/terminal/<store>`
 * (`terminal.ts` is that socket's protocol).
 *
 * ## A process, not an extension `Pseudoterminal`
 *
 * The extension API's `Pseudoterminal` would be the obvious seam, and it is
 * write-only: VS Code tells a terminal *process* how much of its output xterm
 * has drawn (`acknowledgeDataEvent`), and drops that for an extension terminal.
 * Without it there is no flow control, and a build printing faster than the
 * page draws grows the page's memory instead of slowing the build. So the
 * terminal is created through the terminal service with a process of ours, as
 * VS Code's own terminals are.
 *
 * Two ways in, one process class: the tab "Claude · <app>" that this module
 * opens (the `claude=1` link, the status-bar button), and the terminal panel's
 * own "New Terminal", which reaches the backend registered here and starts a
 * session at the store's root.
 */

import * as vscode from "vscode";
import {
  getService,
  type IEditorOverrideServices,
} from "@codingame/monaco-vscode-api";
import { Emitter } from "@codingame/monaco-vscode-api/vscode/vs/base/common/event";
import type { IShellLaunchConfig } from "@codingame/monaco-vscode-api/vscode/vs/platform/terminal/common/terminal";
import { ACTIVE_GROUP } from "@codingame/monaco-vscode-api/vscode/vs/workbench/services/editor/common/editorService";
import type * as TerminalOverride from "@codingame/monaco-vscode-terminal-service-override";

import { api, errorMessage } from "./api";
import { applicationsBuiltFrom } from "./applications";
import {
  TerminalConnection,
  terminalName,
  terminalUrl,
  type ClaudeRequest,
} from "./terminal";

/** The command id, as the manifest contributes it. */
export const CLAUDE_COMMAND = "saltcorn.openClaudeCode";
/** Set when this installation lets the IDE open a terminal. */
export const CLAUDE_CONTEXT = "saltcorn.claudeCodeEnabled";

/** The terminal service, once loaded: what `bootWorkbench` passes to `initialize`. */
export interface TerminalServices {
  readonly services: IEditorOverrideServices;
  readonly override: typeof TerminalOverride;
}

/**
 * Whether this installation lets an admin open a terminal: the
 * `ide_terminal_enabled` setting (Settings → Development).
 *
 * Read before the workbench starts, for the reason the chat's agent listing is:
 * the terminal service draws its panel as a side effect of being imported, so
 * whether there is one has to be known first. A failure to read it costs the
 * terminal and nothing else.
 */
export async function claudeCodeEnabled(): Promise<boolean> {
  try {
    const settings = await api.getSettings();
    return (
      (settings.values as Record<string, unknown>)?.ide_terminal_enabled ===
      true
    );
  } catch (err) {
    console.warn("[saltcorn] the settings could not be read; no terminal", err);
    return false;
  }
}

/**
 * Load the terminal service, with this store's backend registered.
 *
 * **Dynamically imported**, as the chat's services are and for the same two
 * reasons: importing it is what contributes the terminal panel and its
 * commands, which an installation with the terminal turned off must not show;
 * and it is a large part of a bundle such an installation has no use for.
 */
export async function loadTerminalServices(
  store: string
): Promise<TerminalServices> {
  const override = await import(
    "@codingame/monaco-vscode-terminal-service-override"
  );
  const backend = storeBackend(override, store);
  const services = override.default(backend);
  backend.setReady();
  return { services, override };
}

/** Terminal ids, unique for the page. */
let nextTerminalId = 1;

/**
 * A terminal process whose program runs on the server.
 *
 * A class factory rather than a class, because its base class is in the
 * dynamically imported override.
 */
function socketProcessClass(override: typeof TerminalOverride) {
  return class SocketTerminalProcess extends override.SimpleTerminalProcess {
    private readonly exit = new Emitter<number | undefined>();
    private readonly connection: TerminalConnection;

    constructor(url: string, cols: number, rows: number) {
      const data = new Emitter<string>();
      // No pid and no cwd: both are the server's, and VS Code only shows them.
      super(nextTerminalId++, 0, "", data.event);
      this.onProcessExit = this.exit.event;
      this.connection = new TerminalConnection(
        new WebSocket(url),
        { cols, rows },
        {
          data: (text) => data.fire(text),
          exit: (code) => this.exit.fire(code),
        }
      );
      // xterm's mouse reports in the legacy encoding are bytes, not text; the
      // base class leaves this unsupported, which throws on the first click in
      // a program that asked for the mouse.
      this.processBinary = ((bytes: string) => {
        this.connection.binary(bytes);
        return Promise.resolve();
      }) as unknown as typeof this.processBinary;
    }

    async start(): Promise<undefined> {
      await this.connection.started;
      return undefined;
    }
    input(data: string): void {
      this.connection.input(data);
    }
    resize(cols: number, rows: number): void {
      this.connection.resize({ cols, rows });
    }
    // The base class declares this without its argument; VS Code passes the
    // number of characters xterm has drawn since the last call.
    override acknowledgeDataEvent(charCount?: number): void {
      this.connection.acknowledge(charCount ?? 0);
    }
    shutdown(): void {
      this.connection.shutdown();
    }
    // Ctrl-C reaches the program as a byte the terminal turns into SIGINT on the
    // server; there is no other signal to send from here.
    sendSignal(): void {}
    clearBuffer(): void {}
  };
}

/** Where a terminal on `store` connects, from this page. */
function socketUrl(store: string, app: string | null): string {
  return terminalUrl(window.location, store, app);
}

/**
 * The backend the terminal panel's "New Terminal" reaches: a session at the
 * store's root.
 */
function storeBackend(override: typeof TerminalOverride, store: string) {
  const SocketTerminalProcess = socketProcessClass(override);
  return new (class extends override.SimpleTerminalBackend {
    getDefaultSystemShell = async () => "claude";
    createProcess = async (
      shellLaunchConfig: IShellLaunchConfig,
      _cwd: string,
      cols: number,
      rows: number
    ) => {
      // The one setting VS Code reads from the config it is about to launch
      // with rather than from a profile: without it the tab closes the moment
      // the session ends, taking the server's reason (a refusal, a lost
      // connection) with it.
      shellLaunchConfig.waitOnExit ??= true;
      shellLaunchConfig.name ??= terminalName(null);
      return new SocketTerminalProcess(socketUrl(store, null), cols, rows);
    };
  })();
}

/**
 * Open Claude Code on `store` in an editor tab, started in `app`'s source
 * directory when one is given.
 *
 * An editor tab rather than the panel: a session is the work, not a console
 * beside it, and an editor tab can be dragged beside the files it changes.
 */
export async function openClaudeTerminal(
  terminal: TerminalServices,
  store: string,
  app: string | null
): Promise<void> {
  const SocketTerminalProcess = socketProcessClass(terminal.override);
  const service = await getService(terminal.override.ITerminalService);
  const instance = await service.createTerminal({
    config: {
      name: terminalName(app),
      icon: { id: "sparkle" },
      waitOnExit: true,
      customPtyImplementation: (_id, cols, rows) =>
        new SocketTerminalProcess(socketUrl(store, app), cols, rows),
    },
    location: { viewColumn: ACTIVE_GROUP },
  });
  await instance.focusWhenReady(true);
}

/**
 * The command and its status-bar button: another session, in the application
 * this page was opened for, or the one this store holds.
 */
export function registerClaudeCommand(
  terminal: TerminalServices,
  store: string,
  request: ClaudeRequest | null
): vscode.Disposable[] {
  void vscode.commands.executeCommand("setContext", CLAUDE_CONTEXT, true);
  const button = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    98
  );
  button.text = "$(sparkle) Claude";
  button.tooltip = "Open Claude Code in a new tab";
  button.command = CLAUDE_COMMAND;
  button.show();
  const command = vscode.commands.registerCommand(CLAUDE_COMMAND, async () => {
    const app = request?.app ?? (await chooseApplication(store));
    if (app === undefined) return;
    await openClaudeTerminal(terminal, store, app);
  });
  return [button, command];
}

/**
 * Which application's directory to start in: `null` for the store's root,
 * `undefined` when the admin dismissed the choice.
 *
 * A store holding one application starts there without asking; a store holding
 * several asks; one holding none, or whose applications cannot be listed,
 * starts at the root, where Claude Code can still find its way.
 */
async function chooseApplication(
  store: string
): Promise<string | null | undefined> {
  let names: string[];
  try {
    names = applicationsBuiltFrom(await api.listApplications(), store).map(
      (candidate) => candidate.application.name
    );
  } catch (err) {
    console.warn(
      `[saltcorn] ${errorMessage(err, "listing applications failed")}`
    );
    return null;
  }
  if (names.length <= 1) return names[0] ?? null;
  const picked = await vscode.window.showQuickPick(names, {
    placeHolder: "Which application should Claude Code work on?",
  });
  return picked;
}
