// The first-run onboarding, driven through each of its branches with fake
// effects: which dialogs follow which answer, what each one does on the server,
// and where the admin is left.

import { describe, expect, it } from "vitest";

import { checkFlow, choose, settle, startFlow, submit, type FlowState } from "./flow";
import { ONBOARDING, providerFields, REACT, SALTCORN_UI, type OnboardingFx } from "./script";
import { subdomainFor, unusedName } from "./naming";

/** Fake effects that record what they were asked to do. */
function fakes(providers: { name: string; is_default: boolean }[] = []) {
  const calls: string[] = [];
  const fx: OnboardingFx = {
    providers: async () => providers,
    backends: async () => [
      {
        name: "anthropic",
        config_spec: [
          { name: "api_key", label: "API key", type: "text", required: true, options: [], multiline: false },
        ],
      },
      { name: "openai_chat", config_spec: [] },
    ],
    createProvider: async (p) => {
      calls.push(`provider ${p.name} ${p.backend} ${JSON.stringify(p.config)} ${p.model}`);
      return p.name;
    },
    copilot: async (provider) => {
      calls.push(`copilot on ${provider}`);
      return "Admin copilot";
    },
    createApplication: async (app) => {
      calls.push(`app ${app.name} ${app.framework}`);
      return { id: "app-1", name: app.name, ide: app.framework === REACT ? "/ide/?store=my" : null };
    },
    show: (path) => calls.push(`show ${path}`),
  };
  return { fx, calls };
}

/** Pick the option whose label is `label` (untranslated) on the current step. */
function pick(state: FlowState, label: string): FlowState {
  if ("done" in state) throw new Error("already finished");
  const step = ONBOARDING.steps[state.at];
  const choices = step.kind === "ask" ? step.options : step.kind === "say" ? step.buttons : [];
  const offered = choices.filter((c) => !c.when || c.when(state.ctx));
  const index = offered.findIndex((c) => c.label === label);
  if (index < 0) throw new Error(`no "${label}" on ${state.at}`);
  return choose(ONBOARDING, state, index);
}

const at = (state: FlowState) => ("done" in state ? "done" : state.at);

describe("the onboarding script", () => {
  it("is a sound graph", () => {
    expect(checkFlow(ONBOARDING)).toEqual([]);
  });

  it("opens by asking how to build", () => {
    const step = ONBOARDING.steps[ONBOARDING.start];
    expect(step.kind).toBe("ask");
    if (step.kind === "ask") {
      expect(step.options.map((o) => o.label)).toEqual([
        "Generative AI",
        "Point and click",
        "Write code",
      ]);
    }
  });

  describe("Generative AI", () => {
    it("with no provider, sets one up in the dialog and opens the copilot's chat", async () => {
      const { fx, calls } = fakes();
      let state = await settle(ONBOARDING, pick(startFlow(ONBOARDING), "Generative AI"), fx);
      expect(at(state)).toBe("ai-setup");
      const step = ONBOARDING.steps["ai-setup"];
      // The note says how it is done outside the dialogs.
      expect(step.kind === "form" && step.note).toMatch(/Agents → LLM providers/);
      state = await submit(
        ONBOARDING,
        state,
        { name: "house", backend: "anthropic", api_key: " sk-1 ", model: "claude-sonnet-5-5" },
        fx,
      );
      state = await settle(ONBOARDING, state, fx);
      expect(state).toMatchObject({ done: { to: "/agents/Admin%20copilot/chat" } });
      expect(calls).toEqual([
        'provider house anthropic {"api_key":"sk-1"} claude-sonnet-5-5',
        "copilot on house",
      ]);
    });

    it("with a provider, offers to use it", async () => {
      const { fx, calls } = fakes([
        { name: "spare", is_default: false },
        { name: "main", is_default: true },
      ]);
      let state = await settle(ONBOARDING, pick(startFlow(ONBOARDING), "Generative AI"), fx);
      expect(at(state)).toBe("ai-provider");
      expect(state.ctx.vars.provider).toBe("main");
      state = await settle(ONBOARDING, pick(state, "Use {provider}"), fx);
      expect(state).toMatchObject({ done: { to: "/agents/Admin%20copilot/chat" } });
      expect(calls).toEqual(["copilot on main"]);
    });

    it("with a provider, can still set up a new one", async () => {
      const { fx } = fakes([{ name: "main", is_default: true }]);
      let state = await settle(ONBOARDING, pick(startFlow(ONBOARDING), "Generative AI"), fx);
      state = await settle(ONBOARDING, pick(state, "Set up a new provider"), fx);
      expect(at(state)).toBe("ai-setup");
    });
  });

  it("Point and click creates a Saltcorn UI application, selects it, and points at Tables", async () => {
    const { fx, calls } = fakes();
    let state = pick(startFlow(ONBOARDING), "Point and click");
    expect(at(state)).toBe("pnc");
    state = await submit(ONBOARDING, state, { name: " Bookshop " }, fx);
    expect(at(state)).toBe("pnc-table");
    expect(state.ctx.vars.app).toBe("Bookshop");
    expect(calls).toEqual([`app Bookshop ${SALTCORN_UI}`, "show /applications/app-1/views"]);
    expect(pick(state, "Go to Tables")).toMatchObject({ done: { to: "/tables" } });
  });

  describe("Write code", () => {
    it("points an outside coding agent at the MCP settings", () => {
      let state = pick(startFlow(ONBOARDING), "Write code");
      state = pick(state, "With my own coding agent");
      expect(pick(state, "Open the MCP settings")).toMatchObject({
        done: { to: "/settings/development" },
      });
    });

    it("creates a React application and offers its IDE", async () => {
      const { fx, calls } = fakes();
      let state = pick(pick(startFlow(ONBOARDING), "Write code"), "A React application");
      state = await submit(ONBOARDING, state, { name: "Shop" }, fx);
      expect(calls).toEqual([`app Shop ${REACT}`, "show /applications/app-1/edit"]);
      expect(pick(state, "Open the IDE")).toMatchObject({ done: { href: "/ide/?store=my" } });
    });
  });
});

describe("providerFields", () => {
  const backends = [
    {
      name: "anthropic",
      config_spec: [
        { name: "api_key", label: "API key", type: "text", required: true, options: [], multiline: false },
      ],
    },
    { name: "openai_chat", config_spec: [] },
  ];

  it("follows the chosen backend's own settings", () => {
    const names = (backend: string) =>
      providerFields(backends, { backend }).map((f) => f.name);
    expect(names("anthropic")).toEqual(["name", "backend", "api_key", "model"]);
    expect(names("openai_chat")).toEqual(["name", "backend", "model"]);
    // Before one is picked, the first.
    expect(names("")).toEqual(["name", "backend", "api_key", "model"]);
  });
});

describe("the effects' naming", () => {
  it("makes a subdomain of a name", () => {
    expect(subdomainFor("My Application!")).toBe("my-application");
    expect(subdomainFor("Café")).toBe("cafe");
    expect(subdomainFor("???")).toBe("app");
  });

  it("finds an unused name", () => {
    expect(unusedName("Admin copilot", [])).toBe("Admin copilot");
    expect(unusedName("Admin copilot", ["Admin copilot", "Admin copilot 2"])).toBe(
      "Admin copilot 3",
    );
    expect(unusedName("shop", ["shop"], "-")).toBe("shop-2");
  });
});
