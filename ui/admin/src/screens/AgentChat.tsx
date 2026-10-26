// The chat panel: a transcript, a composer, and the agent's run history (§11.4).
//
// Everything about *what* the transcript is lives in `agentChat.ts` — the
// socket, the event fold, the rebuild of a stored run, the composer's controls
// — and is tested there against a stub. This file is the rendering, which is the
// split that lets the interesting half be tested without a browser.
//
// The screen is the one page in the admin that is not a page of cards, because
// a chat is not a document that scrolls: the transcript scrolls *inside* the
// viewport and the composer never moves, which is the shape every hosted agent
// (Kimi, z.ai, and the rest) has converged on and the shape a long tool-using
// turn needs. The measurements are in `admin.css` under "The agent chat".
//
// What is on screen, and why:
//
//   - **A rail of past conversations**, on the left where a chat interface keeps
//     it, rather than a card beside the transcript: it is navigation between
//     conversations, not part of the one being read. Grouped by age, because a
//     list of forty timestamps is not a list anyone reads.
//   - **The person's messages are bubbles; the agent's are not.** The agent's
//     answer is the content of the page. A border around it is a border around
//     everything, and the asymmetry is what makes the two readable at a glance.
//   - **A tool call is one quiet line**, naming the tool, opening onto its
//     arguments and its result. Collapsed because a transcript is read for what
//     the agent *said*; expandable because when it goes wrong, what it did is
//     the only thing that explains it.
//   - **The composer is a capsule with a toolbar row inside it**, and the
//     toolbar is where a trait's own controls go (`ComposerControl`). Nothing
//     declares one yet; the row exists because a mode that modifies the message
//     being written belongs in the box it is being written in, and retrofitting
//     that means redesigning the composer rather than filling in a slot.
//   - **An old run reopens read-only.** It is a record of what happened, and a
//     composer under it would invite an edit to history. Continuing one is a
//     deliberate act — the Continue button, which reconnects the socket to that
//     run.
//
// The same component is also the **popped-out window** (`chatWindows.ts`): the
// pop-out button in the top right hands the conversation to the store and the
// shell re-renders it in the corner, where the rest of the admin can be
// navigated underneath it. Two renderings of one chat, not two chats: the only
// differences are the chrome (a window's title bar carries minimize / full
// screen / close where the page's carries Back and pop-out) and the rail, which
// a 24rem window has no room for and a full-screen one does.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import Alert from "react-bootstrap/Alert";

import { api, errorMessage } from "../api";
import type { GetRunResponse, ListRunsResponse } from "../client";
import {
  ChatSession,
  PANE_WIDTHS,
  agentChatUrl,
  compactionLabel,
  conclusionLabel,
  conclusionNotice,
  emptyChat,
  groupTranscript,
  previewPaneOf,
  resolvePaneUrl,
  splitCodeBlocks,
  transcriptFromRun,
  type ChatState,
  type ComposerControl,
  type Conclusion,
  type ControlValue,
  type DisplayRef,
  type Entry,
  type UserRequest,
  type PreviewPane,
  type SocketLike,
  type ToolEntry as ToolCall,
} from "../agentChat";
import { navigate } from "../App";
import {
  MAX_CHAT_WINDOWS,
  popOutChat,
  useChatWindows,
  type ChatWindowMode,
} from "../chatWindows";
import {
  IconAlertTriangle,
  IconArrowLeft,
  IconArrowUp,
  IconArrowsDiagonal,
  IconArrowsDiagonalMinimize,
  IconChevronDown,
  IconDeviceDesktop,
  IconDeviceMobile,
  IconDeviceTablet,
  IconLayoutColumns,
  IconLayoutSidebar,
  IconMessagePlus,
  IconMinus,
  IconPictureInPicture,
  IconPlayerStop,
  IconRefresh,
  IconRobot,
  IconSparkles,
  IconTool,
  IconTrash,
  IconX,
} from "../icons";
import { StatusBadge, type Tone } from "../layout";
import { RunBar } from "./RunPanel";
import { T, useT } from "../i18n";

type RunItem = ListRunsResponse[number];

/** How a finished run reads in the history, and in the line under a transcript. */
function stateTone(state: string): Tone {
  switch (state) {
    case "done":
      return "green";
    case "failed":
      return "red";
    case "aborted":
      return "yellow";
    default:
      return "blue";
  }
}

/** JSON as a person reads it — a tool's arguments, or a result that happens to
 * be JSON. Anything that is not JSON is shown exactly as it arrived. */
function pretty(value: unknown): string {
  if (typeof value === "string") {
    try {
      return JSON.stringify(JSON.parse(value), null, 2);
    } catch {
      return value;
    }
  }
  return JSON.stringify(value, null, 2) ?? "";
}

/** Which heading a conversation sits under in the rail.
 *
 * Age, not date: the question the rail answers is "the one I had this morning",
 * and four headings answer it where forty timestamps do not. */
function ageGroup(when: Date, now: Date): string {
  const days = Math.floor((startOfDay(now) - startOfDay(when)) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  if (days < 7) return "Previous 7 days";
  if (days < 30) return "Previous 30 days";
  return "Older";
}

function startOfDay(date: Date): number {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

/** The window a popped-out chat is in: how it is showing, and the three things
 * the person can do to it. Absent when the chat is the page. */
export type ChatFrame = {
  mode: ChatWindowMode;
  onMinimize: () => void;
  onRestore: () => void;
  onFullScreen: () => void;
  onClose: () => void;
};

export function AgentChat({
  agent,
  initial,
  frame,
}: {
  agent: string;
  /** The conversation this chat opens on — a popped-out window continuing what
   * the page was showing. A fresh chat when absent. */
  initial?: { runId: string | null; entries: Entry[]; draft?: string };
  frame?: ChatFrame;
}) {
  const { t } = useT();
  const [chat, setChat] = useState<ChatState>(emptyChat());
  const [runs, setRuns] = useState<RunItem[]>([]);
  const [viewing, setViewing] = useState<{ run: RunItem; entries: Entry[] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState(initial?.draft ?? "");
  // The rail starts open where there is room for it beside the transcript, and
  // shut where it would cover it (below Tabler's `lg`, it is a drawer; in a
  // window there is no room for it at all until it goes full screen).
  const [railOpen, setRailOpen] = useState(() => !frame && window.innerWidth >= 992);
  // The pane the agent declares (`preview_pane`), whether it is open, and at
  // which width. Absent for every agent that declares none, which is most.
  const [pane, setPane] = useState<PreviewPane | null>(null);
  const [paneOpen, setPaneOpen] = useState(false);
  const [paneWidth, setPaneWidth] = useState(PANE_WIDTHS[0].name);
  // Bumped to reload the pane. It is the iframe's `key`, so a bump remounts the
  // element: a full page load, which is what "the agent changed the app" means
  // and what a same-origin-only `contentWindow.location.reload()` cannot do
  // across origins anyway.
  const [paneNonce, setPaneNonce] = useState(0);
  // Read once, on the mount that opens the socket: a prop rebuilt on every
  // render of the shell would otherwise reconnect the chat under the person.
  const opening = useRef(initial);
  const session = useRef<ChatSession | null>(null);
  const scroller = useRef<HTMLDivElement | null>(null);
  // Whether the reader is at the bottom of the transcript. A turn that streams
  // for a minute must not yank the view back down while someone is reading what
  // it said two tool calls ago — so it follows only when they were following.
  const following = useRef(true);

  // Which pane this agent carries, read from its stored traits. A failure is
  // silent: the pane is an extra screen, and an agent whose definition could not
  // be read still has a conversation.
  useEffect(() => {
    let cancelled = false;
    api
      .listAgents()
      .then((agents) => {
        if (cancelled) return;
        setPane(previewPaneOf(agents.find((a) => a.name === agent)?.traits));
      })
      .catch(() => {
        if (!cancelled) setPane(null);
      });
    return () => {
      cancelled = true;
      setPane(null);
      setPaneOpen(false);
    };
  }, [agent]);

  const loadRuns = useCallback(async () => {
    try {
      // A subagent's run is read nested inside the run that delegated it, not
      // as a conversation of its own.
      setRuns((await api.listRuns(agent)).filter((run) => !run.parent_run));
    } catch (err) {
      setError(errorMessage(err, "Could not load this agent's history."));
    }
  }, [agent]);

  /** Open a socket for this agent, optionally continuing `run`. */
  const connect = useCallback(
    (run?: { id: string | null; entries: Entry[] }) => {
      session.current?.close();
      // A browser `WebSocket` *is* a `SocketLike` — it sends, it closes, and it
      // calls the four handlers. TypeScript will not agree, because its handler
      // signatures are typed against the DOM's own event objects and a narrower
      // parameter is not assignable under `strictFunctionTypes`. The cast is
      // here rather than in the model, so what the model demands stays the
      // smallest thing a test can be.
      const socket = new WebSocket(agentChatUrl(window.location)) as unknown as SocketLike;
      session.current = new ChatSession(socket, agent, setChat, {
        runId: run?.id ?? null,
        entries: run?.entries ?? [],
      });
      setChat(session.current.current());
      following.current = true;
    },
    [agent],
  );

  useEffect(() => {
    void loadRuns();
    const from = opening.current;
    connect(from ? { id: from.runId, entries: from.entries } : undefined);
    return () => {
      session.current?.close();
      session.current = null;
    };
  }, [agent, connect, loadRuns]);

  // A turn that ended produced or changed a run, so the history is stale.
  const running = chat.running;
  useEffect(() => {
    if (!running) void loadRuns();
  }, [running, loadRuns]);

  // ...and what the agent just did is what the pane is showing, so it is stale
  // too. Only on the *falling* edge, and only while the pane is open: a reload
  // of a hidden frame is a build's worth of requests nobody is looking at.
  const wasRunning = useRef(false);
  useEffect(() => {
    const ended = wasRunning.current && !running;
    wasRunning.current = running;
    if (ended && paneOpen && pane?.reloadOnTurn) setPaneNonce((n) => n + 1);
  }, [running, paneOpen, pane]);

  const entries = viewing ? viewing.entries : chat.entries;

  // After the DOM has grown, not after React has decided to: `scrollHeight` is
  // only right once the new text is laid out. `mode` is in here because a
  // minimized window's transcript is hidden rather than unmounted (it is still
  // a live socket), and a box with no layout has no scroll height to set — so
  // the one that matters is the pass right after it comes back.
  const mode = frame?.mode;
  useLayoutEffect(() => {
    const element = scroller.current;
    if (element && following.current) element.scrollTop = element.scrollHeight;
  }, [entries, running, mode]);

  const onScroll = () => {
    const element = scroller.current;
    if (!element) return;
    following.current =
      element.scrollHeight - element.scrollTop - element.clientHeight < 80;
  };

  const send = () => {
    const text = draft;
    setDraft("");
    following.current = true;
    session.current?.send(text);
  };

  const openRun = async (run: RunItem) => {
    setError(null);
    try {
      const whole = await api.getRun(run.id);
      following.current = true;
      setViewing({ run, entries: transcriptFromRun(whole.context) });
    } catch (err) {
      setError(errorMessage(err, "Could not open that conversation."));
    }
  };

  const continueRun = () => {
    if (!viewing) return;
    connect({ id: viewing.run.id, entries: viewing.entries });
    setViewing(null);
  };

  const removeRun = async (run: RunItem) => {
    if (!window.confirm("Delete this conversation? It is the only record of what happened.")) {
      return;
    }
    try {
      await api.deleteRun(run.id);
      if (viewing?.run.id === run.id) setViewing(null);
      await loadRuns();
    } catch (err) {
      setError(errorMessage(err, "Could not delete that conversation."));
    }
  };

  const newConversation = () => {
    setViewing(null);
    setError(null);
    connect();
  };

  const currentRun = viewing?.run.id ?? chat.runId;

  // The current run as the API serves it, for its cost and its plan: read again
  // whenever a turn ends, which is when both change.
  const [runRecord, setRunRecord] = useState<GetRunResponse | null>(null);
  useEffect(() => {
    if (!currentRun || running) return;
    let cancelled = false;
    api
      .getRun(currentRun)
      .then((found) => {
        if (!cancelled) setRunRecord(found);
      })
      .catch(() => {
        // The bar is extra: a run that cannot be read still has its transcript.
        if (!cancelled) setRunRecord(null);
      });
    return () => {
      cancelled = true;
    };
  }, [currentRun, running]);
  const runBar = runRecord && runRecord.id === currentRun && <RunBar run={runRecord} />;

  // The URL the pane opens on, resolved against this admin's own location: a
  // stored `//todo.{host}` is the application's subdomain on whatever host this
  // browser reached the admin by. `null` for a pane nobody may frame.
  const paneUrl = useMemo(
    () => (pane ? resolvePaneUrl(pane.url, window.location) : null),
    [pane],
  );
  // The pane is the page's screen, not a window's: a 24rem popped-out chat has
  // no room for a column and a browser beside it.
  const splitAvailable = !frame && paneUrl !== null;
  const split = splitAvailable && paneOpen;

  // The rail is 17rem of navigation between conversations: it belongs beside a
  // transcript that has the page, and not inside a window a third that width —
  // nor beside a transcript that is itself down to a column, which is the whole
  // point of the split.
  const railAvailable = (!frame || frame.mode === "full") && !split;
  const showRail = railOpen && railAvailable;

  const railToggle = (
    <button
      type="button"
      className="btn btn-icon btn-ghost-secondary btn-sm"
      aria-label={showRail ? "Hide conversations" : "Show conversations"}
      aria-pressed={showRail}
      title={showRail ? "Hide conversations" : "Show conversations"}
      onClick={() => setRailOpen((open) => !open)}
    >
      <IconLayoutSidebar className="icon-2" />
    </button>
  );

  const status = chat.lastState && chat.lastState !== "done" && !viewing && (
    <StatusBadge tone={stateTone(chat.lastState)}>{chat.lastState}</StatusBadge>
  );

  // Three renderings of one surface: a popped-out window, the page, and the
  // page's left-hand column with the application beside it. `chat-page` is the
  // class that tells the shell this screen has the viewport, so in the split it
  // moves out to the wrapper — the element the shell's `:has(> .chat-page)`
  // rules select must be the wrapper's own child.
  const surfaceClass = frame
    ? "chat-surface"
    : split
      ? "chat-surface chat-split-chat"
      : "chat-page chat-surface";
  const surface = (
    <div className={surfaceClass}>
      {showRail && (
        <ConversationRail
          runs={runs}
          current={currentRun}
          onOpen={(run) => void openRun(run)}
          onDelete={(run) => void removeRun(run)}
          onNew={newConversation}
        />
      )}

      <div className="chat-main">
        {!frame && (
          <div className="chat-topbar">
            {railToggle}
            <button
              type="button"
              className="btn btn-icon btn-ghost-secondary btn-sm"
              aria-label={t("Back to agents")}
              title={t("Back to agents")}
              onClick={() => navigate("/agents")}
            >
              <IconArrowLeft className="icon-2" />
            </button>
            <div className="me-auto overflow-hidden">
              <div className="fw-medium text-truncate">{agent}</div>
            </div>
            {status}
            <button
              type="button"
              className="btn btn-sm btn-outline-secondary"
              onClick={newConversation}
            >
              <IconMessagePlus className="icon-2" />
              <T text="New chat" />
            </button>
            {splitAvailable && (
              <button
                type="button"
                className={
                  split
                    ? "btn btn-icon btn-sm btn-primary"
                    : "btn btn-icon btn-ghost-secondary btn-sm"
                }
                aria-label={split ? "Hide the application" : "Show the application beside the chat"}
                aria-pressed={split}
                title={split ? "Hide the application" : "Show the application beside the chat"}
                onClick={() => setPaneOpen((open) => !open)}
              >
                <IconLayoutColumns className="icon-2" />
              </button>
            )}
            <PopOutButton
              running={chat.running}
              onPopOut={() =>
                popOut({
                  agent,
                  // A past conversation pops out as itself and comes back to
                  // life: a window is somewhere to carry on, and a read-only
                  // one would be a window with nothing to do in it.
                  runId: viewing ? viewing.run.id : chat.runId,
                  entries: viewing ? viewing.entries : chat.entries,
                  draft,
                })
              }
            />
          </div>
        )}

        {runBar}

        <div className="chat-scroll" ref={scroller} onScroll={onScroll}>
          <div className="chat-column py-4">
            {error && (
              <Alert variant="danger" dismissible onClose={() => setError(null)}>
                {error}
              </Alert>
            )}
            {entries.length === 0 && !error && <EmptyTranscript agent={agent} />}
            <TranscriptActions.Provider
              value={{
                runId: viewing ? viewing.run.id : chat.runId,
                awaitingId: viewing ? null : (chat.awaiting?.id ?? null),
                answer: viewing ? undefined : (request, value) => session.current?.answer(request, value),
              }}
            >
              <Transcript entries={entries} />
            </TranscriptActions.Provider>
            {chat.running && !viewing && (
              <div className="chat-thinking mb-3">
                <span className="chat-dot" />
                <span className="chat-dot" />
                <span className="chat-dot" />
                <span className="ms-1"><T text="Working…" /></span>
              </div>
            )}
          </div>
        </div>

        <div className="chat-dock">
          <div className="chat-column">
            {viewing ? (
              <PastConversationBar
                run={viewing.run}
                onContinue={continueRun}
                onNew={newConversation}
              />
            ) : (
              <Composer
                draft={draft}
                running={chat.running}
                controls={chat.controls}
                values={chat.controlValues}
                onDraft={setDraft}
                onControl={(name, value) => session.current?.setControl(name, value)}
                onPress={(name) => session.current?.press(name)}
                onSend={send}
                onAbort={() => session.current?.abort()}
              />
            )}
          </div>
        </div>
      </div>
    </div>
  );

  if (!frame) {
    if (!split || !paneUrl) return surface;
    return (
      <div className="chat-page chat-split">
        {surface}
        <PreviewPaneView
          url={paneUrl}
          nonce={paneNonce}
          width={paneWidth}
          onWidth={setPaneWidth}
          onReload={() => setPaneNonce((n) => n + 1)}
          onClose={() => setPaneOpen(false)}
        />
      </div>
    );
  }

  // A window's title bar is the page's top bar with different furniture, and it
  // stays visible when the body does not — a minimized chat *is* its title bar.
  return (
    <>
      <div className="chat-window-head">
        {frame.mode === "full" && railToggle}
        <div className="chat-window-title text-truncate">{agent}</div>
        {status}
        {frame.mode !== "minimized" && (
          <button
            type="button"
            className="btn btn-icon btn-ghost-secondary btn-sm"
            aria-label={t("New chat")}
            title={t("New chat")}
            onClick={newConversation}
          >
            <IconMessagePlus className="icon-2" />
          </button>
        )}
        {frame.mode === "minimized" ? (
          <button
            type="button"
            className="btn btn-icon btn-ghost-secondary btn-sm"
            aria-label={t("Show this chat")}
            title={t("Show this chat")}
            onClick={frame.onRestore}
          >
            <IconArrowUp className="icon-2" />
          </button>
        ) : (
          <button
            type="button"
            className="btn btn-icon btn-ghost-secondary btn-sm"
            aria-label={t("Minimize this chat")}
            title={t("Minimize")}
            onClick={frame.onMinimize}
          >
            <IconMinus className="icon-2" />
          </button>
        )}
        <button
          type="button"
          className="btn btn-icon btn-ghost-secondary btn-sm"
          aria-label={frame.mode === "full" ? "Leave full screen" : "Full screen"}
          title={frame.mode === "full" ? "Leave full screen" : "Full screen"}
          onClick={frame.mode === "full" ? frame.onRestore : frame.onFullScreen}
        >
          {frame.mode === "full" ? (
            <IconArrowsDiagonalMinimize className="icon-2" />
          ) : (
            <IconArrowsDiagonal className="icon-2" />
          )}
        </button>
        <button
          type="button"
          className="btn btn-icon btn-ghost-secondary btn-sm"
          aria-label={t("Close this chat")}
          title={t("Close")}
          onClick={frame.onClose}
        >
          <IconX className="icon-2" />
        </button>
      </div>
      {surface}
    </>
  );
}

/** The application beside the conversation: a toolbar, and an iframe.
 *
 * The iframe is keyed by `nonce`, so bumping it remounts the element and the
 * page loads again from the top. That is deliberately the crudest reload there
 * is: the pane is cross-origin (the application is on its own subdomain), so
 * there is no `contentWindow` to talk to, and a full load is what "the agent
 * changed the application" means anyway.
 *
 * The width buttons letterbox the frame rather than resizing the pane: what
 * they answer is "does this layout hold up on a phone?", and a 390px column
 * with the rest of the pane left empty is the same question a phone asks.
 */
function PreviewPaneView({
  url,
  nonce,
  width,
  onWidth,
  onReload,
  onClose,
}: {
  url: string;
  nonce: number;
  width: string;
  onWidth: (name: string) => void;
  onReload: () => void;
  onClose: () => void;
}) {
  const { t } = useT();
  const chosen = PANE_WIDTHS.find((w) => w.name === width) ?? PANE_WIDTHS[0];
  return (
    <section className="chat-pane" aria-label={t("The application")}>
      <div className="chat-pane-bar">
        <div className="btn-group btn-group-sm" role="group" aria-label={t("Screen width")}>
          {PANE_WIDTHS.map((option) => (
            <button
              key={option.name}
              type="button"
              className={
                option.name === chosen.name
                  ? "btn btn-icon btn-sm btn-primary"
                  : "btn btn-icon btn-sm btn-outline-secondary"
              }
              aria-label={option.label}
              aria-pressed={option.name === chosen.name}
              title={option.label}
              onClick={() => onWidth(option.name)}
            >
              <PaneWidthIcon name={option.name} />
            </button>
          ))}
        </div>
        <div className="chat-pane-url text-secondary text-truncate">{url}</div>
        <button
          type="button"
          className="btn btn-icon btn-ghost-secondary btn-sm"
          aria-label={t("Reload the application")}
          title={t("Reload")}
          onClick={onReload}
        >
          <IconRefresh className="icon-2" />
        </button>
        <a
          className="btn btn-icon btn-ghost-secondary btn-sm"
          href={url}
          target="_blank"
          rel="noreferrer"
          aria-label={t("Open the application in a new tab")}
          title={t("Open in a new tab")}
        >
          <IconArrowsDiagonal className="icon-2" />
        </a>
        <button
          type="button"
          className="btn btn-icon btn-ghost-secondary btn-sm"
          aria-label={t("Hide the application")}
          title={t("Hide the application")}
          onClick={onClose}
        >
          <IconX className="icon-2" />
        </button>
      </div>
      <div className="chat-pane-stage">
        <iframe
          key={nonce}
          className="chat-pane-frame"
          style={chosen.px === null ? undefined : { width: `${chosen.px}px` }}
          src={url}
          title={t("The application")}
        />
      </div>
    </section>
  );
}

/** The device a width stands for. */
function PaneWidthIcon({ name }: { name: string }) {
  if (name === "phone") return <IconDeviceMobile className="icon-2" />;
  if (name === "tablet") return <IconDeviceTablet className="icon-2" />;
  return <IconDeviceDesktop className="icon-2" />;
}

/** Pop the chat out of the page and into the corner, then leave the page.
 *
 * Leaving is the point of the button rather than a side effect of it: staying
 * would leave the same conversation on screen twice, one of them about to be
 * navigated away from, and the whole reason to pop a chat out is to go
 * somewhere else with it.
 *
 * Refused mid-turn. Popping out closes this socket and opens another, and a
 * turn in flight is answered down the one being closed — so the honest button
 * is one that says to wait rather than one that quietly loses an answer.
 */
function PopOutButton({ running, onPopOut }: { running: boolean; onPopOut: () => void }) {
  const { t } = useT();
  const room = useChatWindows().length < MAX_CHAT_WINDOWS;
  const why = running
    ? "Wait for the agent to finish before popping this chat out"
    : room
      ? "Pop out"
      : `Close one of the ${MAX_CHAT_WINDOWS} popped-out chats first`;
  return (
    <button
      type="button"
      className="btn btn-icon btn-ghost-secondary btn-sm"
      aria-label={t("Pop out this chat")}
      title={why}
      disabled={running || !room}
      onClick={onPopOut}
    >
      <IconPictureInPicture className="icon-2" />
    </button>
  );
}

/** Hand the conversation to the store, and leave the page it was on. */
function popOut(chat: {
  agent: string;
  runId: string | null;
  entries: Entry[];
  draft: string;
}): void {
  popOutChat(chat);
  navigate("/agents");
}

/** The rail: New chat, then this agent's conversations newest first. */
function ConversationRail({
  runs,
  current,
  onOpen,
  onDelete,
  onNew,
}: {
  runs: RunItem[];
  current: string | null;
  onOpen: (run: RunItem) => void;
  onDelete: (run: RunItem) => void;
  onNew: () => void;
}) {
  const { t } = useT();
  const groups = useMemo(() => {
    const now = new Date();
    const out: { title: string; runs: RunItem[] }[] = [];
    for (const run of runs) {
      const title = ageGroup(new Date(run.created_at), now);
      const last = out[out.length - 1];
      if (last?.title === title) last.runs.push(run);
      else out.push({ title, runs: [run] });
    }
    return out;
  }, [runs]);

  return (
    <aside className="chat-history" aria-label={t("Conversations")}>
      <div className="p-2">
        <button type="button" className="btn btn-sm btn-outline-secondary w-100" onClick={onNew}>
          <IconMessagePlus className="icon-2" />
          <T text="New chat" />
        </button>
      </div>
      <div className="chat-history-list">
        {runs.length === 0 && (
          <div className="text-secondary small px-2 py-1"><T text="No conversations yet." /></div>
        )}
        {groups.map((group) => (
          <div key={group.title} className="mt-2">
            <div className="text-secondary text-uppercase fw-bold px-2 mb-1" style={{ fontSize: "0.6875rem" }}>
              {group.title}
            </div>
            {group.runs.map((run) => (
              <div
                key={run.id}
                className={run.id === current ? "chat-history-item active" : "chat-history-item"}
              >
                <button
                  type="button"
                  className="btn btn-link p-0 border-0 text-reset text-decoration-none w-100 text-start"
                  onClick={() => onOpen(run)}
                >
                  <span className="chat-history-title">
                    {run.description || "(no first message)"}
                  </span>
                </button>
                <div className="d-flex align-items-center gap-1 mt-1">
                  <StatusBadge tone={stateTone(run.state)} title={run.error ?? undefined}>
                    {run.state}
                  </StatusBadge>
                  {conclusionLabel(run.conclusion as Conclusion | null) && (
                    <StatusBadge
                      tone="yellow"
                      title={conclusionNotice(run.conclusion as Conclusion | null) ?? undefined}
                    >
                      {conclusionLabel(run.conclusion as Conclusion | null)}
                    </StatusBadge>
                  )}
                  <span className="text-secondary" style={{ fontSize: "0.6875rem" }}>
                    {new Date(run.created_at).toLocaleTimeString([], {
                      hour: "2-digit",
                      minute: "2-digit",
                    })}
                  </span>
                </div>
                <button
                  type="button"
                  className="chat-history-delete"
                  aria-label={t("Delete this conversation")}
                  title={t("Delete this conversation")}
                  onClick={() => onDelete(run)}
                >
                  <IconTrash className="icon-2" />
                </button>
              </div>
            ))}
          </div>
        ))}
      </div>
    </aside>
  );
}

/** Nothing said yet: whose chat this is, and what talking to it means. */
function EmptyTranscript({ agent }: { agent: string }) {
  return (
    <div className="chat-empty">
      <div className="chat-empty-icon">
        <IconRobot className="icon-2" />
      </div>
      <h3 className="mb-1">{agent}</h3>
      <p className="text-secondary mb-0" style={{ maxWidth: "26rem" }}>
        <T text="Ask this agent something. Everything it does — every table it reads, every trigger it runs — happens as you." />
      </p>
    </div>
  );
}

/** The read-only foot of a conversation opened from the history. */
function PastConversationBar({
  run,
  onContinue,
  onNew,
}: {
  run: RunItem;
  onContinue: () => void;
  onNew: () => void;
}) {
  return (
    <div className="chat-composer d-flex align-items-center gap-2 flex-wrap">
      <span className="text-secondary small me-auto">
        <T text="A past conversation, shown as it happened." />{" "}
        <StatusBadge tone={stateTone(run.state)} title={run.error ?? undefined}>
          {run.state}
        </StatusBadge>
      </span>
      <button type="button" className="btn btn-sm btn-ghost-secondary" onClick={onNew}>
        <T text="New chat" />
      </button>
      <button type="button" className="btn btn-sm btn-primary" onClick={onContinue}>
        <T text="Continue this conversation" />
      </button>
    </div>
  );
}

/** The entry box: the text, and under it the toolbar — a trait's controls on
 * the left, send (or stop) on the right, in the same place either way. */
function Composer({
  draft,
  running,
  controls,
  values,
  onDraft,
  onControl,
  onPress,
  onSend,
  onAbort,
}: {
  draft: string;
  running: boolean;
  controls: ComposerControl[];
  values: Record<string, ControlValue>;
  onDraft: (text: string) => void;
  onControl: (name: string, value: ControlValue) => void;
  onPress: (name: string) => void;
  onSend: () => void;
  onAbort: () => void;
}) {
  const { t } = useT();
  const box = useRef<HTMLTextAreaElement | null>(null);

  // Grow with what is typed, up to the height `admin.css` caps it at — a
  // three-line box that scrolls hides the beginning of a long instruction,
  // which is exactly the message worth re-reading before sending.
  useLayoutEffect(() => {
    const element = box.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${element.scrollHeight}px`;
  }, [draft]);

  return (
    <form
      className="chat-composer"
      onSubmit={(e) => {
        e.preventDefault();
        onSend();
      }}
    >
      <textarea
        ref={box}
        rows={1}
        value={draft}
        aria-label={t("Message")}
        placeholder={running ? "Waiting for the agent…" : "Ask the agent…"}
        disabled={running}
        onChange={(e) => onDraft(e.target.value)}
        // Enter sends, Shift+Enter is a newline — the convention every chat box
        // in the world has taught.
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            onSend();
          }
        }}
      />
      <div className="chat-composer-bar">
        <div className="chat-composer-controls">
          {controls.map((control) => (
            <ComposerControlView
              key={control.name}
              control={control}
              value={values[control.name]}
              disabled={running}
              onChange={onControl}
              onPress={onPress}
            />
          ))}
        </div>
        <span className="chat-hint ms-auto d-none d-sm-inline">
          {running ? "Running" : "Enter to send"}
        </span>
        {running ? (
          <button
            type="button"
            className="chat-send chat-send-stop"
            aria-label={t("Stop the agent")}
            title={t("Stop")}
            onClick={onAbort}
          >
            <IconPlayerStop className="icon-2" />
          </button>
        ) : (
          <button
            type="submit"
            className="chat-send"
            aria-label={t("Send")}
            title={t("Send")}
            disabled={draft.trim() === ""}
          >
            <IconArrowUp className="icon-2" />
          </button>
        )}
      </div>
    </form>
  );
}

/** One trait-declared control, rendered from its declaration alone — the panel
 * knows the two kinds, never which trait sent them (§11.2's rule for the trait
 * config form, in the place a trait speaks to the person mid-conversation). */
function ComposerControlView({
  control,
  value,
  disabled,
  onChange,
  onPress,
}: {
  control: ComposerControl;
  value: ControlValue | undefined;
  disabled: boolean;
  onChange: (name: string, value: ControlValue) => void;
  onPress: (name: string) => void;
}) {
  if (control.kind === "button") {
    // Acts at once: sends its label as the message, with itself set.
    return (
      <button
        type="button"
        className="chat-control-toggle"
        title={control.title}
        disabled={disabled}
        onClick={() => onPress(control.name)}
      >
        {control.label}
      </button>
    );
  }
  if (control.kind === "send") {
    // Acts at once: the chosen option is sent as the message, and the select
    // shows its prompt again.
    return (
      <select
        className="chat-control-select"
        aria-label={control.label ?? control.name}
        title={control.title}
        disabled={disabled}
        value=""
        onChange={(e) => {
          if (e.target.value !== "") onChange(control.name, e.target.value);
        }}
      >
        <option value="">{control.label ?? "Send…"}</option>
        {control.options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    );
  }
  if (control.kind === "toggle") {
    const on = value === true;
    return (
      <button
        type="button"
        className="chat-control-toggle"
        aria-pressed={on}
        title={control.title}
        onClick={() => onChange(control.name, !on)}
      >
        {control.label}
      </button>
    );
  }
  return (
    <select
      className="chat-control-select"
      aria-label={control.label ?? control.name}
      title={control.title}
      value={typeof value === "string" ? value : control.options[0].value}
      onChange={(e) => onChange(control.name, e.target.value)}
    >
      {control.options.map((option) => (
        <option key={option.value} value={option.value}>
          {control.label ? `${control.label}: ${option.label}` : option.label}
        </option>
      ))}
    </select>
  );
}

/** The transcript's entries, with a run of calls to one tool drawn as one
 * badge. */
export function Transcript({ entries }: { entries: Entry[] }) {
  const items = useMemo(() => groupTranscript(entries), [entries]);
  return (
    <>
      {items.map((item) =>
        item.kind === "tools" ? (
          <ToolGroup key={`tools-${item.index}`} name={item.name} calls={item.calls} />
        ) : (
          <TranscriptEntry key={`entry-${item.index}`} entry={item.entry} />
        ),
      )}
    </>
  );
}

/** What the transcript's interactive entries need from the chat around them:
 * the run their parts belong to, the question it waits on, and how to answer.
 * A read-only transcript (a run's page, a past conversation) has no `answer`. */
export const TranscriptActions = createContext<{
  runId: string | null;
  awaitingId: string | null;
  answer?: (request: UserRequest, value: unknown) => void;
}>({ runId: null, awaitingId: null });

/** What a tool showed the person (TODO-agent-skills AS6.3): each part fetched
 * by id and drawn — rows as a table, an image, a player. */
function DisplayParts({ parts }: { parts: DisplayRef[] }) {
  return (
    <div className="chat-turn">
      {parts.map((part) => (
        <DisplayPartView key={part.id} part={part} />
      ))}
    </div>
  );
}

function DisplayPartView({ part }: { part: DisplayRef }) {
  const { runId } = useContext(TranscriptActions);
  const [loaded, setLoaded] = useState<{ data: unknown; content_base64?: string | null } | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  useEffect(() => {
    if (!runId) return;
    let cancelled = false;
    api
      .getRunPart(runId, part.id)
      .then((got) => {
        if (!cancelled) setLoaded(got);
      })
      .catch((err) => {
        if (!cancelled) setFailed(errorMessage(err, `Could not load ${part.summary}.`));
      });
    return () => {
      cancelled = true;
    };
  }, [runId, part.id, part.summary]);

  if (failed) return <div className="small text-danger">{failed}</div>;
  if (!loaded) return <div className="small text-muted">{part.summary}</div>;
  const source = `data:${part.mime_type};base64,${loaded.content_base64 ?? ""}`;
  if (part.kind === "image" && /^image\/(jpeg|png|gif|webp)$/.test(part.mime_type)) {
    return <img src={source} alt={part.summary} className="d-block my-2 border rounded" style={{ maxWidth: "100%" }} />;
  }
  if (part.kind === "audio") {
    return <audio controls src={source} className="d-block my-2" aria-label={part.summary} />;
  }
  const rows = loaded.data as { title?: string | null; columns?: string[]; rows?: Record<string, unknown>[] };
  const columns = rows.columns ?? Object.keys(rows.rows?.[0] ?? {});
  return (
    <div className="my-2">
      {rows.title && <div className="fw-semibold small mb-1">{rows.title}</div>}
      <div className="table-responsive border rounded">
        <table className="table table-sm mb-0">
          <thead>
            <tr>
              {columns.map((column) => (
                <th key={column}>{column}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {(rows.rows ?? []).map((row, index) => (
              <tr key={index}>
                {columns.map((column) => (
                  <td key={column}>{pretty(row[column])}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/** A question the agent asked (AS6.1): answerable while the run waits on it,
 * and only in the live chat. */
function RequestCard({ request }: { request: UserRequest }) {
  const { t } = useT();
  const { awaitingId, answer } = useContext(TranscriptActions);
  const open = answer !== undefined && awaitingId === request.id;
  const [picked, setPicked] = useState<string[]>([]);
  const [text, setText] = useState("");

  return (
    <div className="chat-turn">
      <div className="border rounded p-3 mb-3">
        <div className="mb-2" style={{ whiteSpace: "pre-wrap" }}>
          {request.prompt}
        </div>
        {!open ? (
          <div className="small text-muted">
            <T text="This question is no longer waiting for an answer." />
          </div>
        ) : request.kind === "choice" ? (
          <div className="d-flex flex-column gap-2">
            {request.options.map((option) =>
              request.multiple ? (
                <label key={option.value} className="form-check">
                  <input
                    type="checkbox"
                    className="form-check-input"
                    checked={picked.includes(option.value)}
                    onChange={(e) =>
                      setPicked((p) =>
                        e.target.checked ? [...p, option.value] : p.filter((v) => v !== option.value),
                      )
                    }
                  />
                  <span className="form-check-label">
                    {option.label || option.value}
                    {option.description && <div className="small text-muted">{option.description}</div>}
                  </span>
                </label>
              ) : (
                <button
                  key={option.value}
                  type="button"
                  className="btn btn-outline-primary text-start"
                  onClick={() => answer?.(request, option.value)}
                >
                  {option.label || option.value}
                  {option.description && <div className="small text-muted">{option.description}</div>}
                </button>
              ),
            )}
            {request.multiple && (
              <button
                type="button"
                className="btn btn-primary align-self-start"
                disabled={picked.length === 0}
                onClick={() => answer?.(request, picked)}
              >
                <T text="Answer" />
              </button>
            )}
          </div>
        ) : request.kind === "text" ? (
          <form
            className="d-flex gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              if (text.trim() !== "") answer?.(request, text);
            }}
          >
            <input
              className="form-control"
              value={text}
              placeholder={request.placeholder}
              aria-label={t("Your answer")}
              onChange={(e) => setText(e.target.value)}
            />
            <button type="submit" className="btn btn-primary" disabled={text.trim() === ""}>
              <T text="Answer" />
            </button>
          </form>
        ) : request.kind === "approve" ? (
          <div className="d-flex gap-2">
            <button type="button" className="btn btn-primary" onClick={() => answer?.(request, true)}>
              {request.label}
            </button>
            <button type="button" className="btn btn-outline-secondary" onClick={() => answer?.(request, false)}>
              <T text="Decline" />
            </button>
          </div>
        ) : (
          <button type="button" className="btn btn-primary" onClick={() => answer?.(request, true)}>
            {request.label}
          </button>
        )}
      </div>
    </div>
  );
}

/** One entry of the transcript. */
function TranscriptEntry({ entry }: { entry: Entry }) {
  if (entry.kind === "display") {
    return <DisplayParts parts={entry.parts} />;
  }
  if (entry.kind === "request") {
    return <RequestCard request={entry.request} />;
  }
  if (entry.kind === "user") {
    return (
      <div className="chat-turn chat-turn-user">
        <div className="chat-bubble">{entry.text}</div>
      </div>
    );
  }
  if (entry.kind === "assistant") {
    return (
      <div className="chat-turn">
        {entry.reasoning && (
          <details className="chat-reasoning">
            <summary>
              <IconSparkles className="icon-2" />
              <T text="Reasoning" />
            </summary>
            <div className="chat-reasoning-text">{entry.reasoning}</div>
          </details>
        )}
        <AgentText text={entry.text} />
      </div>
    );
  }
  if (entry.kind === "compaction") {
    // A quiet divider rather than an alert: nothing went wrong, and the
    // transcript above it is whole. The summary is what the model saw instead
    // of the older turns, so it opens for anyone asking why the agent forgot.
    return (
      <details className="chat-compaction">
        <summary>
          <IconArrowsDiagonalMinimize className="icon-2" />
          {compactionLabel(entry)}
        </summary>
        <div className="chat-compaction-text">
          {entry.summary ?? "Only old tool results were cleared; the model still saw every turn."}
        </div>
      </details>
    );
  }
  if (entry.kind === "notice") {
    return (
      <Alert variant="warning" className="py-2 d-flex align-items-start gap-2">
        <IconAlertTriangle className="icon-2 flex-shrink-0 mt-1" />
        <div className="text-break small">{entry.message}</div>
      </Alert>
    );
  }
  if (entry.kind === "error") {
    // In the transcript, where it happened: a chat that silently stopped would
    // be unfixable by the person watching it.
    return (
      <Alert variant="danger" className="py-2 d-flex align-items-start gap-2">
        <IconAlertTriangle className="icon-2 flex-shrink-0 mt-1" />
        <div className="text-break small">{entry.message}</div>
      </Alert>
    );
  }
  return <ToolEntry entry={entry} />;
}

/** What the agent said: prose as typed, fenced code as code. */
function AgentText({ text }: { text: string }) {
  const blocks = useMemo(() => splitCodeBlocks(text), [text]);
  if (blocks.length === 0) return null;
  return (
    <>
      {blocks.map((block, i) =>
        block.kind === "prose" ? (
          <div key={i} className="chat-prose">
            {block.text}
          </div>
        ) : (
          <CodeBlock key={i} language={block.language} text={block.text} />
        ),
      )}
    </>
  );
}

/** A fenced block, with the one thing anyone does to code on a web page. */
function CodeBlock({ language, text }: { language: string; text: string }) {
  const [copied, setCopied] = useState(false);

  const copy = () => {
    void navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1500);
      },
      () => setCopied(false),
    );
  };

  return (
    <div className="chat-code">
      <div className="chat-code-head">
        <span>{language || "code"}</span>
        <button type="button" className="btn btn-sm btn-ghost-secondary py-0" onClick={copy}>
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre>{text}</pre>
    </div>
  );
}

/** Consecutive calls to one tool: one badge with a count, opening onto each
 * call as its own badge. */
function ToolGroup({ name, calls }: { name: string; calls: ToolCall[] }) {
  const [open, setOpen] = useState(false);
  const running = calls.some((call) => call.result === null);
  const failed = calls.some((call) => call.isError);

  return (
    <div className="chat-tool">
      <button
        type="button"
        className="chat-tool-head"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        {failed ? (
          <IconAlertTriangle className="icon-2 text-danger flex-shrink-0" />
        ) : (
          <IconTool className="icon-2 flex-shrink-0" />
        )}
        <span className="chat-tool-name">{name}</span>
        <span className="chat-tool-count">({calls.length})</span>
        {running && <RunningDots />}
        <IconChevronDown className="icon-2 chat-tool-caret flex-shrink-0" />
      </button>
      {open ? (
        <div className="chat-tool-group">
          {calls.map((call) => (
            <ToolEntry key={call.id} entry={call} />
          ))}
        </div>
      ) : (
        // Screenshots stay on screen while the calls are folded, as they do
        // for a single call.
        calls.flatMap((call) => call.images ?? []).map((src, index) => (
          <ToolImage key={index} src={src} name={name} />
        ))
      )}
    </div>
  );
}

function ToolImage({ src, name }: { src: string; name: string }) {
  return (
    <img
      src={src}
      alt={`what ${name} saw`}
      className="chat-tool-image d-block mt-1 border rounded"
      style={{ maxWidth: "100%" }}
    />
  );
}

/** A tool call: the line, and what it opens onto. */
function ToolEntry({ entry }: { entry: ToolCall }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const running = entry.result === null;

  return (
    <div className="chat-tool">
      <button
        type="button"
        className="chat-tool-head"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        {entry.isError ? (
          <IconAlertTriangle className="icon-2 text-danger flex-shrink-0" />
        ) : (
          <IconTool className="icon-2 flex-shrink-0" />
        )}
        <span className="chat-tool-name">{entry.name}</span>
        {running && <RunningDots />}
        <IconChevronDown className="icon-2 chat-tool-caret flex-shrink-0" />
      </button>
      {/* A screenshot is what the agent saw: shown, not folded away with the
          snapshot text (TODO §7b). */}
      {entry.images?.map((src, index) => (
        <ToolImage key={index} src={src} name={entry.name} />
      ))}
      {open && (
        <div className="chat-tool-body">
          <Labelled label={t("Arguments")}>
            <pre>{pretty(entry.args)}</pre>
          </Labelled>
          <Labelled label={t("Result")}>
            <pre className={entry.isError ? "text-danger" : undefined}>
              {running ? "(still running)" : pretty(entry.result)}
            </pre>
          </Labelled>
        </div>
      )}
    </div>
  );
}

function RunningDots() {
  const { t } = useT();
  return (
    <span className="chat-thinking" aria-label={t("running")}>
      <span className="chat-dot" />
      <span className="chat-dot" />
      <span className="chat-dot" />
    </span>
  );
}

function Labelled({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="mb-1">
      <div className="text-secondary" style={{ fontSize: "0.6875rem" }}>
        {label}
      </div>
      {children}
    </div>
  );
}
