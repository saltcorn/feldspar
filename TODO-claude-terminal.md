# Saltcorn v2 — Claude Code in the IDE (milestones CT1–CT5)

One click from an application opens the IDE with Claude Code running in a terminal tab, in the
application's source directory, already connected to this server over MCP. Only file stores
with a local path are supported. The design is
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §12.1, "Claude Code in a terminal tab".
Milestones are prefixed **CT**; work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## CT1 — The terminal bridge

- [x] CT1.1 `ide_terminal_enabled` in the Development section of `_fd_config`, off by default,
      read per request (`sc_config`, like `McpSettings`). A value that is not a boolean reads as
      off.
- [x] CT1.2 `ServerConfig::claude_cli` (default `claude` on `PATH`), next to `language_server`,
      with the matching `feldspar serve` option.
- [x] CT1.3 `crates/sc-server/src/terminal.rs`: `GET /ide/terminal/{store}?app=<name>`, a PTY
      (`pty-process`), binary frames for terminal bytes, text control frames for
      `{"resize": …}`. Working directory: the app's source directory, otherwise the store root.
      Refusals: admin check before the upgrade; feature off, no store, no local path, no
      executable and too many sessions (`MAX_TERMINALS`) in the close frame. The program, its session
      and its descendants are ended when the socket closes.
- [x] CT1.4 Tests (`sc-server/tests/ide_terminal.rs`), with `sh` standing in for `claude`: a
      non-admin is refused; every refusal arrives with its reason; bytes round-trip; a resize
      reaches the PTY (`stty size`); the process is gone after close; the session limit holds;
      the working directory is the app's source directory.

## CT2 — The terminal in the IDE

- [x] CT2.1 `@codingame/monaco-vscode-terminal-service-override` in `ui/ide`, loaded only when
      `ide_terminal_enabled` is on, and a terminal process bridged to the CT1 socket (bytes,
      resize, a close reason printed into the terminal).
- [x] CT2.2 `claude=1&app=<name>` on the IDE URL opens "Claude · <app>" as an editor-area tab
      once the workbench is up, then removes the parameter with `history.replaceState`. A
      status-bar item opens another session.
- [x] CT2.3 Flow control: `{"pause": true}` / `{"pause": false}` text frames, on which the
      bridge stops and resumes reading the PTY; the IDE sends them from VS Code's own
      acknowledgements of drawn output, past a high and at a low mark.
- [x] CT2.4 Tests: the terminal connection against a fake socket (input, output, resize, close
      reason, pause and resume); the URL handling (opens once, parameter removed).

## CT3 — Wiring Claude Code to this server

- [ ] CT3.1 Session tokens: an in-memory map from token to (user, session), filled by the
      terminal route and emptied when its socket closes. The MCP route checks it before the token
      table, accepts a session token only from loopback, and accepts it even when `mcp_enabled`
      is off (the token table is still not read in that case).
- [ ] CT3.2 Launch line: `--continue`, `--mcp-config` (a temporary file outside the store,
      deleted with the session), `--strict-mcp-config`, and `--append-system-prompt` with the
      framework's builder-agent prompt. `CLAUDE_CONFIG_DIR=<data dir>/claude`; the Anthropic
      provider's key as `ANTHROPIC_API_KEY` when one is configured.
- [ ] CT3.3 Tests: a session token opens MCP while the socket lives and not after; it is refused
      from a non-loopback peer; it works with `mcp_enabled` off while a table token does not;
      nothing is written into the store; the launch line and environment are what the design says.
- [ ] CT3.4 The installation's MCP servers: an `offer_to_terminal` attribute on `_fd_mcp_servers`
      (a checkbox on the MCP server form, off by default), and `/ide/mcp-proxy/{server}`, which
      takes only a live session token and forwards to the server's URL with its stored headers,
      through `sc-mcp`'s guard and timeout. The session's MCP config gets one entry per offered
      streamable-HTTP server. SSE servers are left out, and the terminal says which ones and why.
- [ ] CT3.5 Tests (with a stub MCP server): a call through the proxy arrives with the stored
      headers; the config file contains no header value; a server that is not offered, or a
      table token, or a closed session's token is refused; a private-network URL is refused
      unless the definition allows it; an SSE server is left out with a reason.

## CT4 — One click from the admin UI

- [ ] CT4.1 `listApplications` reports, per application, whether a Claude session can start and
      why not (feature off, no source directory, no local path, no executable).
- [ ] CT4.2 "Build with Claude" in the application sidebar (`appNav.ts`) and in the
      applications list, next to the "Edit code" links; disabled with the reason as tooltip.
      Strings in every locale.
- [ ] CT4.3 Tests: the availability reasons (`sc-api`); the links and their disabled state
      (`ui/admin`).

## CT5 — Keeping the workbench in step

- [ ] CT5.1 While a terminal is attached, the bridge watches its working directory (`notify`,
      debounced, ignoring `node_modules`, `.git` internals and build output) and sends
      `{"changed": [paths]}` frames.
- [ ] CT5.2 The IDE passes them to `announceChanged` and refreshes source control when the git
      state moved.
- [ ] CT5.3 Tests: a file written in the directory produces one debounced frame naming it;
      ignored directories produce none; an open editor shows the new contents.

### Try it

Turn on `ide_terminal_enabled`. On a React application in a local store, click **Build with
Claude**. The IDE opens with Claude at its prompt in the app's directory. Ask it to add a field
to a table and show it on a page: the column appears in the catalog (MCP), the file changes appear
in the open editor, and the build succeeds. Reload the page: the conversation continues.

## Open questions

- Keep a session alive after its tab closes (scrollback buffer, re-attach by session id), or is
  `--continue` enough?
- Auto-update the CLI, as the Home Assistant add-on does, or leave installing it to the operator?
- Run each session in its own cgroup? Ending a session lists its processes before `SIGHUP` and
  again before `SIGKILL`, and kills both lists. A process from the first list that exits during
  the half-second in between could have its PID reused by an unrelated process, which would then
  be killed. That is unlikely (Linux hands out PIDs in order up to about four million before
  reusing one), but not impossible. A cgroup would close this, and would also hold a process
  that daemonises on purpose, which escapes today.
- Proxy SSE MCP servers too (rewriting the posting URL in the event stream), or wait until
  someone configures one they want in a session?
