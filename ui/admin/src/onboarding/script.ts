// The first-run onboarding, in the dialog language of `flow.ts`.
//
// Shown once, to the first admin to sign in (`authStatus.onboarding_pending`),
// and never again once finished or skipped (`finishOnboarding`). It asks how
// the admin wants to build and then does the setup that answer needs, each
// dialog saying at the top where the same thing is done without it:
//
//   - **Generative AI** — an LLM provider (the one already there, or a new one
//     set up in a form here), then an admin copilot on it, and its chat opened.
//   - **Point and click** — a Saltcorn UI application, selected in the sidebar,
//     and a pointer to the first table.
//   - **Write code** — the MCP settings for an outside coding agent, or a React
//     application whose code opens in the IDE.
//
// Everything that reaches the server is an `OnboardingFx` effect, so this file
// is the *what* and `effects.ts` the *how*; `script.test.ts` drives it with
// fakes.

import type { FieldSpec } from "../settings";
import { ask, finish, flow, form, run, say, t } from "./flow";

/** An LLM provider, as the dialogs need it. */
export type ProviderChoice = { name: string; is_default: boolean };

/** A provider backend and the settings it declares. */
export type BackendChoice = { name: string; config_spec: FieldSpec[] };

/** What the onboarding does on the server. */
export interface OnboardingFx {
  providers(): Promise<ProviderChoice[]>;
  backends(): Promise<BackendChoice[]>;
  /** Create a provider with one chat model and make it the default; answers its name. */
  createProvider(provider: {
    name: string;
    backend: string;
    config: Record<string, unknown>;
    model: string;
  }): Promise<string>;
  /** An agent with the `admin_copilot` trait on `provider`, made if there is
   * none; answers its name. */
  copilot(provider: string): Promise<string>;
  /** Create an application of `framework`; answers its id, name and, for one
   * with a source tree, where its code opens in the IDE. */
  createApplication(app: {
    name: string;
    framework: string;
  }): Promise<{ id: string; name: string; ide: string | null }>;
  /** Show a hash route behind the dialogs (which selects an application in the
   * sidebar). */
  show(path: string): void;
}

/** The frameworks the two "make an application" branches create. */
export const SALTCORN_UI = "saltcorn-ui";
export const REACT = "react";

/** A plain required text field. */
function text(name: string, label: string, sublabel?: string): FieldSpec {
  return { name, label, type: "text", required: true, options: [], multiline: false, sublabel };
}

/** The setup form's own fields, around the chosen backend's settings. */
export function providerFields(
  backends: BackendChoice[],
  values: Record<string, string>,
): FieldSpec[] {
  const chosen = backends.find((b) => b.name === values.backend) ?? backends[0];
  return [
    text("name", t("Name"), t("How agents refer to this provider.")),
    {
      ...text("backend", t("Backend")),
      options: backends.map((b) => b.name),
      default: backends[0]?.name,
    },
    ...(chosen?.config_spec ?? []),
    text(
      "model",
      t("Model"),
      t("The model's name as the provider knows it, for example claude-sonnet-5-5. More models can be added on the provider's page later."),
    ),
  ];
}

/** The name an application form starts with. */
const APP_FIELDS = [text("name", t("Application name"), t("You can rename it at any time."))];

export const ONBOARDING = flow<OnboardingFx>({
  start: "welcome",
  steps: {
    welcome: ask({
      title: t("Welcome to Saltcorn"),
      text: t("Let's get your first application started. How do you want to build?"),
      options: [
        {
          label: t("Generative AI"),
          description: t("Describe what you want, and the admin copilot builds the tables, actions and applications."),
          go: "ai",
        },
        {
          label: t("Point and click"),
          description: t("Design tables, views and pages yourself, with no code."),
          go: "pnc",
        },
        {
          label: t("Write code"),
          description: t("Build with your own coding agent, or write a React application in the browser IDE."),
          go: "code",
        },
      ],
    }),

    // --- Generative AI --------------------------------------------------------

    ai: run({
      busy: t("Looking for an LLM provider…"),
      exits: ["ai-provider", "ai-setup-load"],
      do: async (_ctx, fx) => {
        const providers = await fx.providers();
        const preferred = providers.find((p) => p.is_default) ?? providers[0];
        return preferred
          ? { go: "ai-provider", set: { provider: preferred.name } }
          : "ai-setup-load";
      },
    }),
    "ai-provider": ask({
      title: t("Choose an LLM provider"),
      text: t("The admin copilot needs a large language model to talk to. This installation already has one set up: {provider}."),
      options: [
        { label: t("Use {provider}"), description: t("Start building straight away."), go: "ai-copilot" },
        {
          label: t("Set up a new provider"),
          description: t("Connect another API key, or a model server of your own."),
          go: "ai-setup-load",
        },
      ],
    }),
    "ai-setup-load": run({
      busy: t("Loading the provider backends…"),
      exits: ["ai-setup"],
      do: async (_ctx, fx) => ({ go: "ai-setup", data: { backends: await fx.backends() } }),
    }),
    "ai-setup": form({
      title: t("Set up an LLM provider"),
      note: t("Outside this guide, LLM providers are set up in the sidebar under Agents → LLM providers → New provider, and their models are added on the provider's page."),
      text: t("Building with generative AI needs a large language model. Connect one with its API key, or point Saltcorn at a model server you run."),
      fields: (ctx, values) => providerFields(backendsOf(ctx), values),
      initial: () => ({ name: "default" }),
      submitLabel: t("Set up provider"),
      exits: ["ai-copilot"],
      submit: async (values, ctx, fx) => {
        const backends = backendsOf(ctx);
        const backend = values.backend || backends[0]?.name || "";
        const spec = backends.find((b) => b.name === backend)?.config_spec ?? [];
        const config: Record<string, unknown> = {};
        for (const field of spec) {
          const value = values[field.name] ?? "";
          if (value.trim() !== "") config[field.name] = value.trim();
        }
        const name = await fx.createProvider({
          name: values.name.trim(),
          backend,
          config,
          model: (values.model ?? "").trim(),
        });
        return { go: "ai-copilot", set: { provider: name } };
      },
    }),
    "ai-copilot": run({
      busy: t("Getting the admin copilot ready…"),
      exits: [],
      do: async (ctx, fx) => ({
        go: finish({ to: "/agents/{agent}/chat" }),
        set: { agent: await fx.copilot(ctx.vars.provider) },
      }),
    }),

    // --- Point and click -------------------------------------------------------

    pnc: form({
      title: t("Create your application"),
      note: t("Outside this guide, applications are created in the sidebar: Applications → New application, choosing Saltcorn UI as the framework."),
      text: t("A Saltcorn UI application is built from views and pages that you design in the browser."),
      fields: () => APP_FIELDS,
      initial: () => ({ name: "My application" }),
      submitLabel: t("Create application"),
      exits: ["pnc-table"],
      submit: async (values, _ctx, fx) => {
        const app = await fx.createApplication({ name: values.name.trim(), framework: SALTCORN_UI });
        // Behind the dialog, so the sidebar has it selected when they close.
        fx.show(`/applications/${encodeURIComponent(app.id)}/views`);
        return { go: "pnc-table", set: { app: app.name, appId: app.id } };
      },
    }),
    "pnc-table": say({
      title: t("Now create your first table"),
      text: t("{app} is created, and selected in the sidebar under Applications, where its views and pages are. Views show and edit the rows of a table, so the next step is a table to keep your data in: open Tables under Data Layer and press + New table."),
      buttons: [{ label: t("Go to Tables"), go: finish({ to: "/tables" }) }],
    }),

    // --- Write code ------------------------------------------------------------

    code: ask({
      title: t("How do you want to write code?"),
      options: [
        {
          label: t("With my own coding agent"),
          description: t("Claude Code, Codex or another agent builds through Saltcorn's MCP server."),
          go: "code-mcp",
        },
        {
          label: t("A React application"),
          description: t("A React and TypeScript project, edited in the browser IDE and built on this server."),
          go: "code-react",
        },
      ],
    }),
    "code-mcp": say({
      title: t("Connect your coding agent"),
      note: t("This is in the sidebar under Settings → Development."),
      text: t("Saltcorn runs an MCP server that a coding agent connects to. Switch it on and mint a token under Settings → Development, then give your agent the address and the token shown there."),
      buttons: [{ label: t("Open the MCP settings"), go: finish({ to: "/settings/development" }) }],
    }),
    "code-react": form({
      title: t("Create a React application"),
      note: t("Outside this guide, applications are created in the sidebar: Applications → New application, choosing React as the framework."),
      text: t("Saltcorn writes the project, with a client typed against your tables, and builds it on this server."),
      fields: () => APP_FIELDS,
      initial: () => ({ name: "My application" }),
      submitLabel: t("Create application"),
      exits: ["code-react-done"],
      submit: async (values, _ctx, fx) => {
        const app = await fx.createApplication({ name: values.name.trim(), framework: REACT });
        fx.show(`/applications/${encodeURIComponent(app.id)}/edit`);
        return {
          go: "code-react-done",
          set: { app: app.name, appId: app.id },
          data: { ide: app.ide },
        };
      },
    }),
    "code-react-done": say({
      title: t("Your code is ready"),
      text: t("{app} is created and selected in the sidebar, and its first build has started. Its project opens in the IDE from Edit code under the application in the sidebar."),
      buttons: [
        {
          label: t("Open the IDE"),
          when: (ctx) => typeof ctx.data.ide === "string",
          go: finish((ctx) => ({ href: ctx.data.ide as string })),
        },
        { label: t("Close"), variant: "secondary", go: finish() },
      ],
    }),
  },
});

function backendsOf(ctx: { data: Record<string, unknown> }): BackendChoice[] {
  return (ctx.data.backends as BackendChoice[] | undefined) ?? [];
}
