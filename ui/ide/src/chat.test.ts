/**
 * The chat relay: which agents this store offers, and what one turn of a
 * conversation does (design §12.1, §11.4).
 *
 * Three layers, none of which needs a workbench — the same split the rest of
 * this project relies on. Which agents belong to a store is a filter over two
 * listings the API already serves. What the panel offers is a function of those
 * agents. And a turn is a socket protocol, so it is tested against a socket that
 * is a plain object, exactly as `ui/admin`'s chat model is.
 */

import { describe, expect, it, vi } from "vitest";

import {
  AgentConversation,
  agentChatUrl,
  type ServerEvent,
  type SocketLike,
} from "./agentChat";
import {
  changedPaths,
  diffstatPaths,
  hasChanges,
  mergeChanges,
  participantContributions,
  storeModel,
  uniqueSlug,
  relayEvent,
  toolProgress,
  workspacePath,
  type ResponseStream,
} from "./chatRelay";
import { codingAgentsForStore, type StoreAgent } from "./codingAgents";
import type { ListAgentsResponse, ListApplicationsResponse } from "./client";

/** An agent as `listAgents` returns one, with only what the match reads. */
function agent(
  name: string,
  traits: { trait: string; config: unknown }[],
  over: Partial<ListAgentsResponse[number]> = {},
): ListAgentsResponse[number] {
  return {
    id: `id-${name}`,
    name,
    description: `${name} does things`,
    provider: "openai",
    model: "gpt-5",
    system_prompt: "",
    traits,
    min_role: 1,
    attributes: {},
    error: null,
    ...over,
  };
}

/** An application as `listApplications` returns one. */
function application(
  name: string,
  source: { store: string; path: string } | null,
): ListApplicationsResponse[number] {
  return {
    id: `id-${name}`,
    name,
    description: "",
    subdomain: name,
    framework: { name: "react", config: {} },
    extra_frameworks: [],
    tables: [],
    file_stores: [],
    triggers: [],
    streams: [],
    apis: [],
    static_dirs: [],
    csp: null,
    attributes: {},
    source,
    builds: true,
    has_views: false,
  };
}

/** The `coding` trait's configuration, as the builder agent is created with it. */
function coding(
  store: string,
  root: string,
  mayEdit = true,
): { trait: string; config: unknown } {
  return { trait: "coding", config: { store, root, may_edit: mayEdit } };
}

describe("which agents a store has", () => {
  it("is the ones whose coding trait names this store", () => {
    const agents = [
      agent("build-todo", [
        coding("todoapp", "app"),
        { trait: "build_application", config: {} },
      ]),
      agent("build-blog", [coding("blogapp", "")]),
      agent("copilot", [
        { trait: "admin_copilot", config: { allow_edit: true } },
      ]),
    ];
    const found = codingAgentsForStore(agents, [], "todoapp");
    expect(found.map((one) => one.name)).toEqual(["build-todo"]);
    expect(found[0].root).toBe("app");
    expect(found[0].mayEdit).toBe(true);
  });

  it("names the application built from the agent's own directory", () => {
    const agents = [agent("build-todo", [coding("todoapp", "app")])];
    const apps = [application("todo", { store: "todoapp", path: "./app/" })];
    expect(codingAgentsForStore(agents, apps, "todoapp")[0].application).toBe(
      "todo",
    );
    // …and not an application built from a *different* directory of the same
    // store, which is the case a store-only match would get wrong.
    const elsewhere = [
      application("other", { store: "todoapp", path: "site" }),
    ];
    expect(
      codingAgentsForStore(agents, elsewhere, "todoapp")[0].application,
    ).toBeNull();
  });

  it("puts an application's agent first, then sorts by name", () => {
    const agents = [
      agent("zeta", [coding("todoapp", "")]),
      agent("alpha", [coding("todoapp", "")]),
      agent("build-todo", [coding("todoapp", "app")]),
    ];
    const apps = [application("todo", { store: "todoapp", path: "app" })];
    expect(
      codingAgentsForStore(agents, apps, "todoapp").map((one) => one.name),
    ).toEqual(["build-todo", "alpha", "zeta"]);
  });

  it("keeps an agent that cannot run, with its reason", () => {
    const broken = agent("build-todo", [coding("todoapp", "")], {
      error: "no LLM provider named `gpt`",
    });
    const found = codingAgentsForStore([broken], [], "todoapp");
    expect(found).toHaveLength(1);
    expect(found[0].error).toBe("no LLM provider named `gpt`");
  });

  it("offers nothing for a store no agent works on", () => {
    expect(
      codingAgentsForStore(
        [agent("build-todo", [coding("todoapp", "")])],
        [],
        "assets",
      ),
    ).toHaveLength(0);
  });
});

describe("what the chat panel is given", () => {
  const agents: StoreAgent[] = [
    {
      name: "build-todo",
      description: "Builds the `todo` application",
      root: "app",
      mayEdit: true,
      application: "todo",
      error: null,
    },
    {
      name: "Read Only Reviewer",
      description: "",
      root: "",
      mayEdit: false,
      application: null,
      error: null,
    },
  ];

  it("is one participant per agent, the application's first and default", () => {
    const participants = participantContributions(agents);
    expect(participants.map((one) => one.name)).toEqual([
      "build-todo",
      "read-only-reviewer",
    ]);
    expect(participants[0].isDefault).toBe(true);
    expect(participants[1].isDefault).toBeUndefined();
    expect(participants[0].fullName).toBe("todo");
  });

  it("scopes a participant's id to the extension that contributes it", () => {
    expect(participantContributions(agents)[0].id).toBe(
      "saltcorn.saltcorn-agents.build-todo",
    );
  });

  it("says whether an agent may write, because a read-only one will refuse", () => {
    const participants = participantContributions(agents);
    expect(participants[0].description).toContain("reads and changes app/");
    expect(participants[1].description).toContain("may not change files");
  });

  it("keeps two agents that slug alike apart", () => {
    const taken = new Set<string>();
    expect(uniqueSlug("Build Todo", taken)).toBe("build-todo");
    expect(uniqueSlug("build/todo", taken)).toBe("build-todo-2");
    expect(uniqueSlug("!!!", taken)).toBe("agent");
  });

  it("offers exactly one model, and does not pretend it is a choice", () => {
    const model = storeModel("todoapp");
    expect(model.isDefault).toBe(true);
    expect(model.isUserSelectable).toBe(false);
    expect(model.capabilities.toolCalling).toBe(false);
  });
});

/** A response stream that records what was written to it. */
function recordingStream(): ResponseStream & {
  markdownText: string[];
  progressText: string[];
  thinkingText: string[];
} {
  const markdownText: string[] = [];
  const progressText: string[] = [];
  const thinkingText: string[] = [];
  return {
    markdownText,
    progressText,
    thinkingText,
    markdown: (value) => markdownText.push(value),
    progress: (value) => progressText.push(value),
    thinkingProgress: (delta) => thinkingText.push(delta.text),
  };
}

describe("one event, relayed", () => {
  it("writes text as markdown and reasoning as thinking", () => {
    const stream = recordingStream();
    relayEvent({ type: "text", delta: "Hello" }, stream);
    relayEvent({ type: "reasoning", delta: "hmm" }, stream);
    expect(stream.markdownText).toEqual(["Hello"]);
    expect(stream.thinkingText).toEqual(["hmm"]);
  });

  it("drops reasoning where the thinking part is not available", () => {
    const stream = recordingStream();
    const withoutThinking: ResponseStream = {
      markdown: stream.markdown,
      progress: stream.progress,
    };
    relayEvent({ type: "reasoning", delta: "hmm" }, withoutThinking);
    expect(stream.markdownText).toEqual([]);
  });

  it("reports a tool call as progress and says nothing about a tool that worked", () => {
    const stream = recordingStream();
    relayEvent(
      {
        type: "tool_call",
        id: "1",
        name: "read_file_todoapp_app",
        arguments: { path: "App.tsx" },
      },
      stream,
    );
    relayEvent(
      {
        type: "tool_result",
        id: "1",
        name: "read_file_todoapp_app",
        content: "…",
        is_error: false,
      },
      stream,
    );
    expect(stream.progressText).toEqual(["Reading App.tsx"]);
    expect(stream.markdownText).toEqual([]);
  });

  it("reports a compaction as progress and keeps its summary out of the answer", () => {
    const stream = recordingStream();
    const compaction = { step: 4, elided: 2, before_tokens: 3400, after_tokens: 1200 };
    relayEvent({ type: "compaction", ...compaction }, stream);
    relayEvent({ type: "compaction", ...compaction, summary: "## Goal\nship" }, stream);
    expect(stream.progressText).toEqual([
      "Clearing old tool output from the context",
      "Summarising the conversation so far to fit the context",
    ]);
    expect(stream.markdownText).toEqual([]);
  });

  it("puts a failed tool, and an error, in the answer where they happened", () => {
    const stream = recordingStream();
    relayEvent(
      {
        type: "tool_result",
        id: "1",
        name: "find_files_todoapp_app",
        content: 'not found: "app"',
        is_error: true,
      },
      stream,
    );
    relayEvent({ type: "error", message: "the provider refused" }, stream);
    expect(stream.markdownText.join("")).toContain(
      '`find_files_todoapp_app` failed: not found: "app"',
    );
    expect(stream.markdownText.join("")).toContain("the provider refused");
  });

  it("says which file a write changed, and nothing for a read", () => {
    const stream = recordingStream();
    const wrote = relayEvent(
      {
        type: "tool_call",
        id: "1",
        name: "write_file_todoapp_app",
        arguments: { path: "src/App.tsx", content: "…" },
      },
      stream,
    );
    const read = relayEvent(
      {
        type: "tool_call",
        id: "2",
        name: "read_file_todoapp_app",
        arguments: { path: "x.ts" },
      },
      stream,
    );
    expect(wrote).toEqual({ paths: ["src/App.tsx"], everything: false, committed: false });
    expect(read).toEqual({ paths: [], everything: false, committed: false });
  });

  it("drops everything after a shell command, and shows the command", () => {
    const stream = recordingStream();
    const ran = relayEvent(
      {
        type: "tool_call",
        id: "1",
        name: "shell_todoapp_app",
        arguments: { command: "npm install zod" },
      },
      stream,
    );
    expect(stream.progressText).toEqual(["Running `npm install zod`"]);
    expect(ran.everything).toBe(true);
    expect(hasChanges(ran)).toBe(true);
    expect(
      toolProgress("process_todoapp_app", { action: "start", name: "dev", command: "npm run dev" }),
    ).toBe("Process dev: start `npm run dev`");
    expect(toolProgress("process_todoapp_app", { action: "list" })).toBe("Process: list");
    // A managed process's output is not a write this relay can see; only the
    // shell's call is taken to have changed the tree.
    expect(
      relayEvent(
        {
          type: "tool_call",
          id: "2",
          name: "process_todoapp_app",
          arguments: { action: "stop", name: "dev" },
        },
        stream,
      ).everything,
    ).toBe(false);
  });

  it("shows the checks and the planner's tools as progress", () => {
    expect(toolProgress("check_todoapp_app", {})).toBe("Running the checks");
    expect(toolProgress("save_plan_todoapp_app", { features: [] })).toBe("Saving the plan");
    expect(toolProgress("implement_feature_todoapp_app", { id: "filter" })).toBe(
      "Implementing feature filter",
    );
    expect(toolProgress("explore_todoapp_app", { question: "Where are routes?" })).toBe(
      "Exploring: Where are routes?",
    );
    expect(changedPaths("check_todoapp_app", {})).toEqual([]);
  });

  it("says which files a feature's session changed, and whether it committed", () => {
    const stream = recordingStream();
    const content = [
      "feature `filter`: done",
      "session: run 1234, completed after 6 steps",
      "summary:",
      "M looks like a diffstat | but is the summary",
      "check: green, no new failures.",
      "commit: 3f2a9c1 Add a filter to the task list",
      "diffstat:",
      "R src/Old.tsx → src/List.tsx",
      "M src/App.tsx | +3 -1",
      "A src/filter.ts | +20 -0",
      "D src/unused.ts | +0 -8",
      "4 files changed, 23 insertions(+), 9 deletions(-)",
      "diff:",
      "M src/not-a-path.ts | +1 -1",
    ].join("\n");
    const change = relayEvent(
      { type: "tool_result", id: "1", name: "implement_feature_todoapp_app", content, is_error: false },
      stream,
    );
    expect(change.paths).toEqual([
      "src/Old.tsx",
      "src/List.tsx",
      "src/App.tsx",
      "src/filter.ts",
      "src/unused.ts",
    ]);
    expect(change.committed).toBe(true);
    // Nothing of the result lands in the answer: the planner reads it, and says
    // what matters in its own words.
    expect(stream.markdownText).toEqual([]);

    const uncommitted = relayEvent(
      {
        type: "tool_result",
        id: "2",
        name: "implement_feature_todoapp_app",
        content: "feature `x`: failed\ncommit: none, nothing changed.\ndiff: nothing changed.",
        is_error: false,
      },
      stream,
    );
    expect(uncommitted).toEqual({ paths: [], everything: false, committed: false });
    expect(diffstatPaths("no diffstat here")).toEqual([]);
  });

  it("merges a turn's changes into one announcement", () => {
    const merged = mergeChanges([
      { paths: ["a.ts"], everything: false, committed: false },
      { paths: ["a.ts", "b.ts"], everything: false, committed: true },
      { paths: [], everything: false, committed: false },
    ]);
    expect(merged).toEqual({ paths: ["a.ts", "b.ts"], everything: false, committed: true });
    expect(hasChanges(mergeChanges([]))).toBe(false);
  });

  it("says every file a patch changes", () => {
    const patch = [
      "*** Begin Patch",
      "*** Update File: src/App.tsx",
      "*** Move to: src/Main.tsx",
      "-a",
      "+b",
      "*** Add File: src/new.ts",
      "+x",
      "*** Delete File: src/old.ts",
      "*** End Patch",
    ].join("\n");
    expect(changedPaths("apply_patch_todoapp_app", { patch })).toEqual([
      "src/App.tsx",
      "src/Main.tsx",
      "src/new.ts",
      "src/old.ts",
    ]);
    expect(toolProgress("apply_patch_todoapp_app", { patch })).toBe(
      "Patching src/App.tsx, src/Main.tsx, src/new.ts, src/old.ts",
    );
  });

  it("shows a look at the application as progress with its path", () => {
    expect(toolProgress("view_app_todoapp_app", { action: "goto", path: "/tasks" })).toBe(
      "Looking at /tasks",
    );
    expect(toolProgress("view_app_todoapp_app", { action: "click", ref: "@e3" })).toBe(
      "Looking at the application",
    );
    expect(changedPaths("view_app_todoapp_app", { path: "/tasks" })).toEqual([]);
  });

  it("shows listing the application's assets, and writes nothing", () => {
    expect(toolProgress("list_assets_todoapp_app", {})).toBe(
      "Listing the application's assets",
    );
    expect(toolProgress("list_assets_todoapp_app", { pattern: "*.png" })).toBe(
      "Listing assets: *.png",
    );
    expect(changedPaths("list_assets_todoapp_app", { pattern: "*.png" })).toEqual([]);
  });

  it("names a tool it does not know rather than inventing a verb for it", () => {
    expect(toolProgress("query_rows_books", { table: "books" })).toBe(
      "query_rows_books",
    );
    expect(
      toolProgress("search_files_todoapp_app", { pattern: "useState" }),
    ).toBe("Searching for useState");
    expect(changedPaths("edit_file_todoapp_app", {})).toEqual([]);
  });

  it("places a changed file under the agent's own root, not the workspace's", () => {
    expect(workspacePath("todoapp", "app", "src/App.tsx")).toBe(
      "/todoapp/app/src/App.tsx",
    );
    expect(workspacePath("todoapp", "", "src/App.tsx")).toBe(
      "/todoapp/src/App.tsx",
    );
  });
});

/** Let every pending microtask run — the conversation opens its socket through
 * a promise chain, so "the turn has been sent" is two ticks away, not one. */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** A socket that records what was sent and lets a test answer it. */
function fakeSocket(): SocketLike & {
  sent: string[];
  receive: (event: ServerEvent) => void;
} {
  const socket: SocketLike & {
    sent: string[];
    receive: (event: ServerEvent) => void;
  } = {
    sent: [],
    send: (data) => socket.sent.push(data),
    close: vi.fn(),
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    receive: (event) => socket.onmessage?.({ data: JSON.stringify(event) }),
  };
  return socket;
}

describe("a conversation with an agent", () => {
  it("binds the socket to the agent, then sends the turn", async () => {
    const socket = fakeSocket();
    const conversation = new AgentConversation("build-todo", () =>
      Promise.resolve(socket),
    );
    const events: ServerEvent[] = [];
    const turn = conversation.ask("hello", (event) => events.push(event));
    await flush();
    expect(JSON.parse(socket.sent[0])).toEqual({
      type: "start",
      agent: "build-todo",
      run: null,
    });
    expect(JSON.parse(socket.sent[1])).toEqual({
      type: "message",
      text: "hello",
    });

    socket.receive({ type: "text", delta: "hi" });
    socket.receive({ type: "done", run: "run-1", state: "done", answer: "hi" });
    expect(await turn).toEqual({ run: "run-1", state: "done" });
    // `done` ends the turn and is not an event the caller has to know about.
    expect(events).toEqual([{ type: "text", delta: "hi" }]);
  });

  it("carries the run into the next turn, so the agent keeps its history", async () => {
    const socket = fakeSocket();
    const conversation = new AgentConversation("build-todo", () =>
      Promise.resolve(socket),
    );
    const first = conversation.ask("one", () => {});
    await flush();
    socket.receive({ type: "done", run: "run-1", state: "done", answer: "" });
    await first;
    expect(conversation.runId).toBe("run-1");

    // The socket drops; the next turn opens another and names the run.
    socket.onclose?.();
    const reconnected = fakeSocket();
    const carried = new AgentConversation("build-todo", () =>
      Promise.resolve(reconnected),
    );
    void carried.ask("two", () => {});
    await flush();
    expect(JSON.parse(reconnected.sent[0]).agent).toBe("build-todo");
  });

  it("ends the turn when the socket closes, rather than waiting for a `done` that cannot come", async () => {
    const socket = fakeSocket();
    const conversation = new AgentConversation("build-todo", () =>
      Promise.resolve(socket),
    );
    const events: ServerEvent[] = [];
    const turn = conversation.ask("hello", (event) => events.push(event));
    await flush();
    socket.onclose?.();
    expect(await turn).toEqual({ run: null, state: "disconnected" });
    expect(events[0]).toEqual({
      type: "error",
      message: "The connection closed before the agent finished.",
    });
  });

  it("sends an abort when the request is cancelled", async () => {
    const socket = fakeSocket();
    const conversation = new AgentConversation("build-todo", () =>
      Promise.resolve(socket),
    );
    let cancel = () => {};
    const token = {
      isCancellationRequested: false,
      onCancellationRequested: (listener: () => void) => {
        cancel = listener;
        return { dispose: () => {} };
      },
    };
    const turn = conversation.ask("hello", () => {}, token);
    await flush();
    cancel();
    expect(JSON.parse(socket.sent[2])).toEqual({ type: "abort" });
    socket.receive({
      type: "done",
      run: "run-1",
      state: "aborted",
      answer: "",
    });
    expect((await turn).state).toBe("aborted");
  });

  it("drops a frame it cannot read rather than losing the turn with it", async () => {
    const socket = fakeSocket();
    const conversation = new AgentConversation("build-todo", () =>
      Promise.resolve(socket),
    );
    const events: ServerEvent[] = [];
    const turn = conversation.ask("hello", (event) => events.push(event));
    await flush();
    socket.onmessage?.({ data: "not json" });
    socket.receive({ type: "done", run: null, state: "done", answer: "" });
    await turn;
    expect(events).toEqual([]);
  });
});

describe("where the socket is", () => {
  it("is wss: from a page that is https:, or the browser blocks it", () => {
    expect(agentChatUrl({ protocol: "http:", host: "localhost:3032" })).toBe(
      "ws://localhost:3032/admin/agent-chat",
    );
    expect(agentChatUrl({ protocol: "https:", host: "example.com" })).toBe(
      "wss://example.com/admin/agent-chat",
    );
  });
});
