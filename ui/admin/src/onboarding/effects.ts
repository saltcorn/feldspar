// The onboarding's effects (`OnboardingFx`) over the admin API: what each
// dialog's answer does on the server. Kept apart from the script so the script
// can be driven by fakes, and so each effect makes the same requests the
// ordinary screen for it would — a provider and its model as `LlmProviderForm`
// and `LlmModels` make them, an application as `ApplicationForm` does.

import { api } from "../api";
import { ideUrl, navigate } from "../App";
import { noteApplicationsChanged } from "../appActions";
import type { CreateApplicationRequest } from "../client";
import { NEW_LOCAL_FILE_STORE } from "../newFileStore";
import { buildConfig, initialValues } from "../settings";
import { subdomainFor, unusedName } from "./naming";
import type { OnboardingFx } from "./script";

/** The trait that makes an agent the admin copilot. */
export const ADMIN_COPILOT_TRAIT = "admin_copilot";

export const onboardingEffects: OnboardingFx = {
  providers: () => api.listLlmProviders(),

  backends: () => api.listLlmProviderBackends(),

  async createProvider({ name, backend, config, model }) {
    if (model === "") throw new Error("Name the model the provider should use.");
    const created = await api.createLlmProvider({ name, description: "", backend, config });
    try {
      await api.createLlmModel(created.id, {
        name: model,
        description: "",
        kind: "chat",
        is_default: true,
        config: {},
      });
    } catch (err) {
      // A provider with no model cannot answer, and left behind it would make
      // the corrected second attempt fail on its name.
      await api.deleteLlmProvider(created.id).catch(() => {});
      throw err;
    }
    // What the admin just set up is what they mean to use: a new application's
    // builder agent picks the default.
    await api.setDefaultLlmProvider(created.id);
    return created.name;
  },

  async copilot(provider) {
    const agents = await api.listAgents();
    const existing = agents.find(
      (a) => a.provider === provider && a.traits.some((t) => t.trait === ADMIN_COPILOT_TRAIT),
    );
    if (existing) return existing.name;
    const traits = await api.listAgentTraits();
    const spec = traits.find((t) => t.name === ADMIN_COPILOT_TRAIT)?.config_spec;
    if (!spec) throw new Error("This server has no admin copilot to set up.");
    const created = await api.createAgent({
      name: unusedName(
        "Admin copilot",
        agents.map((a) => a.name),
      ),
      description: "Builds this installation's tables, actions and applications.",
      provider,
      model: null,
      system_prompt: "",
      traits: [{ trait: ADMIN_COPILOT_TRAIT, config: buildConfig(spec, initialValues(spec, {})) }],
      min_role: 1,
      attributes: {},
    });
    return created.name;
  },

  async createApplication({ name, framework }) {
    const [frameworks, apps] = await Promise.all([api.listFrameworks(), api.listApplications()]);
    const chosen = frameworks.find((f) => f.name === framework);
    if (!chosen) throw new Error(`This server has no ${framework} framework.`);
    const config = buildConfig(chosen.config_spec, initialValues(chosen.config_spec, {}));
    // Somewhere to keep the code, made for it, as the form's "Create a new
    // local file store" choice does.
    for (const setting of chosen.file_store_settings) config[setting] = NEW_LOCAL_FILE_STORE;
    const body: CreateApplicationRequest = {
      name,
      description: "",
      subdomain: unusedName(
        subdomainFor(name),
        apps.map((a) => a.subdomain),
        "-",
      ),
      framework: { name: framework, config },
      extra_frameworks: [],
      tables: [],
      api_tables: [],
      file_stores: [],
      triggers: [],
      streams: [],
      // REST at `/api`, as a new application on the form starts with.
      apis: [{ provider: "rest", mount: "/api", config: {} }],
      static_dirs: [],
      uploads: [],
      agents: [],
      csp: undefined,
      attributes: {},
      min_role: 100,
      max_role: 1,
      access_if: "",
    };
    const created = await api.createApplication(body);
    noteApplicationsChanged();
    return {
      id: created.id,
      name: created.name,
      ide: created.source ? ideUrl(created.source.store) : null,
    };
  },

  show: (path) => navigate(path),
};
