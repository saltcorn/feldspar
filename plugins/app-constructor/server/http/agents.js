// The application a route names, and the construction agent: which application it may build,
// which LLM provider it uses, and making and deleting it.

"use strict";

const { invalid, notFound } = require("../rules/model.js");

/** The trait that gives the construction agent its planning tools, and the built-in one that
 * builds the data model. */
const CONSTRUCTOR_TRAIT = "constructor";
const DATA_MODEL_TRAIT = "data_model";
/** The application setting naming the LLM provider Plan & build uses. */
const CONSTRUCTOR_PROVIDER = "constructor_provider";
const REACT = "react";

const NEEDS_CONSOLE =
  'the App Constructor runs its agents as an administrator at the console, and this module was not granted that; an admin allows it on the Modules tab ("Agents": console)';

/** The name the construction agent of `app` is created under: next to its builder agent, and
 * stable under a rename of the application. */
const constructionAgentName = (app) =>
  `constructor-${String(app.subdomain).trim()}`;

async function requireApp(env, raw) {
  const app = await env.apps.get(raw);
  if (!app) throw notFound(`no application with id ${raw}`);
  return app;
}

/** Refuse an application the constructor cannot build: anything but React. A feature task is
 * the builder agent's, and the other frameworks' builders are not what the prompts are for. */
function requireConstructible(app) {
  if (app.framework !== REACT) {
    throw invalid(
      `the App Constructor builds React applications; \`${app.name}\` uses the \`${app.framework}\` framework`
    );
  }
}

/** The LLM provider `app`'s settings choose for Plan & build, if any. */
function chosenProvider(app) {
  const chosen = app.settings && app.settings[CONSTRUCTOR_PROVIDER];
  return typeof chosen === "string" && chosen.trim() ? chosen.trim() : null;
}

/** The provider called `name`, refused unless it has a default chat model to plan and build
 * with. */
async function usableProvider(env, name) {
  const provider = (await env.agents.providers()).find((p) => p.name === name);
  if (!provider) throw notFound(`there is no LLM provider \`${name}\``);
  if (!provider.model)
    throw invalid(
      `LLM provider \`${provider.name}\` has no default model; mark one of its models as the default`
    );
  return provider;
}

/** Create the agent that plans `app`'s construction and builds its data model, answering its
 * name. An agent already under that name is used as it is: an admin's edits to it survive a
 * construction deleted and started again. The provider is the one named (by the request, else
 * the application's settings), else the builder agent's, so planning and building talk to the
 * model the admin already chose, else the installation's default. */
async function createConstructionAgent(env, app, named) {
  const name = constructionAgentName(app);
  if (await env.agents.get(name)) return name;
  let provider;
  if (named) {
    provider = await usableProvider(env, named);
  } else {
    const builder = await env.agents.get(app.builder_agent);
    if (builder) {
      provider = await usableProvider(env, builder.provider);
    } else {
      const fallback = (await env.agents.providers()).find((p) => p.default);
      if (!fallback)
        throw invalid(
          "no LLM provider is connected; the App Constructor needs one to plan and build"
        );
      provider = await usableProvider(env, fallback.name);
    }
  }
  await env.agents.save({
    name,
    provider: provider.name,
    description: `Plans and builds the data model of \`${app.name}\``,
    system_prompt: `You construct the \`${
      app.name
    }\` application (subdomain \`${String(
      app.subdomain
    ).trim()}\`). You plan its construction in phases and build its data model: tables, fields and the application's queries. Its code is built by its builder agent, \`${
      app.builder_agent
    }\`.`,
    traits: [
      {
        trait: CONSTRUCTOR_TRAIT,
        config: { application: String(app.subdomain).trim() },
      },
      { trait: DATA_MODEL_TRAIT, config: {} },
    ],
  });
  return name;
}

/** Before planning or running: if the application's settings choose a provider and the
 * construction agent uses another, switch it to that one (with its default model), so a change
 * on the form applies. */
async function useChosenProvider(env, app, construction) {
  const chosen = chosenProvider(app);
  if (!chosen || !construction.agent) return;
  const agent = await env.agents.get(construction.agent);
  if (!agent || agent.provider === chosen) return;
  const provider = await usableProvider(env, chosen);
  await env.agents.save({
    name: agent.name,
    provider: provider.name,
    model: null,
  });
}

/** Delete the agent `construction` was made with, if it is still that: named for this
 * application and carrying the constructor trait. One an admin renamed, re-purposed or made
 * stays. Answers its name when it went. */
async function deleteConstructionAgent(agents, app, construction) {
  const name = constructionAgentName(app);
  if (construction.agent !== name) return null;
  const agent = await agents.get(name);
  if (
    !agent ||
    !(agent.traits || []).some((t) => t.trait === CONSTRUCTOR_TRAIT)
  )
    return null;
  try {
    return (await agents.remove(name)).deleted ? name : null;
  } catch (_) {
    // Not made by this module: an admin's agent, which stays.
    return null;
  }
}

module.exports = {
  CONSTRUCTOR_TRAIT,
  CONSTRUCTOR_PROVIDER,
  NEEDS_CONSOLE,
  constructionAgentName,
  requireApp,
  requireConstructible,
  chosenProvider,
  createConstructionAgent,
  useChosenProvider,
  deleteConstructionAgent,
};
