// Plan & build is the bundled App Constructor module (`plugins/app-constructor`):
// its screen is a module screen on each React application, and its routes are
// the module's. What the rest of the admin needs of it — whether it is
// installed, installing it from the bundled catalog, its constructions for
// Home, and creating one for "Application from a description" — is here.

import { api } from "./api";
import type { ListModulesResponse } from "./client";
import { moduleRoutes, moduleScreenPath, routeJson } from "./moduleScreens";

/** The module's package name. */
export const CONSTRUCTOR_MODULE = "@feldspar/app-constructor";
/** Where its routes and screens are (the package name's last part). */
export const CONSTRUCTOR_SEGMENT = "app-constructor";
/** Its application screen. */
export const CONSTRUCTOR_SCREEN = "construct";

/** An application's Plan & build screen. */
export function constructPath(appId: string): string {
  return moduleScreenPath(appId, CONSTRUCTOR_SEGMENT, CONSTRUCTOR_SCREEN);
}

/** One construction as the module's `GET /constructions` lists it. */
export type ConstructionSummary = {
  id: number | string;
  application: string;
  status: string;
  phases: number;
  phases_done: number;
};

/** Whether the module is installed and loaded; if not, the bundled catalog's
 * id to install it by (`null`: this server bundles no App Constructor). */
export type ConstructorState = { installed: true } | { installed: false; bundled: string | null };

export function constructorState(listing: ListModulesResponse): ConstructorState {
  if (listing.modules.some((m) => m.name === CONSTRUCTOR_MODULE && m.loaded)) {
    return { installed: true };
  }
  const entry = listing.bundled.find((b) => b.name === CONSTRUCTOR_MODULE);
  return { installed: false, bundled: entry?.id ?? null };
}

/** The module's answer to `path`, with the session and CSRF header. */
export function constructorJson(path: string, init?: RequestInit): Promise<unknown> {
  return routeJson(moduleRoutes(CONSTRUCTOR_SEGMENT), path, init);
}

/** Every construction; none when the module is not installed. */
export async function listConstructions(): Promise<ConstructionSummary[]> {
  if (!constructorState(await api.listModules()).installed) return [];
  return (await constructorJson("/constructions")) as ConstructionSummary[];
}

/** Install the module from the bundled catalog (the catalog's manifest grants
 * its permissions); nothing when it is already installed. */
export async function installConstructor(): Promise<void> {
  const state = constructorState(await api.listModules());
  if (state.installed) return;
  if (!state.bundled) {
    throw new Error("This server does not bundle the App Constructor module, so Plan & build cannot be installed here.");
  }
  const installed = await api.installModule({ source: "bundled", location: state.bundled });
  if (!installed.loaded) {
    throw new Error(
      `The App Constructor module was installed but did not load${
        installed.issues.length ? `: ${installed.issues.join("; ")}` : ""
      }. See Settings → Modules.`,
    );
  }
}

/** Create the construction of the application `appId` on `provider`. */
export async function createConstruction(appId: string, provider: string): Promise<void> {
  await constructorJson(`/applications/${encodeURIComponent(appId)}/construction`, {
    method: "POST",
    body: JSON.stringify(provider ? { provider } : {}),
  });
}
