/**
 * Saltcorn's file-store IDE (design §12.1).
 *
 * A page, not a screen: VS Code initializes once per page and cannot be unloaded,
 * so the workbench gets its own bundle and its own route (`/ide/?store=<name>`)
 * outside the admin SPA. The store is chosen by the query parameter, which is also
 * why there is no client-side router here — switching stores is a navigation.
 */
import "./style.css";
import { api, errorStatus } from "./api";
import { requestedPath } from "./storeFiles";
import type { FileStoreSummary } from "./git";

const CONTAINER_ID = "workbench";

/** The store this page edits, or `null` when the URL does not name one. */
function requestedStore(): string | null {
  const store = new URL(window.location.href).searchParams.get("store")?.trim();
  return store == null || store === "" ? null : store;
}

/**
 * The container the workbench renders into.
 *
 * `index.html` carries it, but the server also serves a bootstrap document (for a
 * request that arrives before the bundle's `index.html` is in place), so the
 * element is created when it is missing rather than assumed.
 */
function workbenchContainer(): HTMLElement {
  const existing = document.getElementById(CONTAINER_ID);
  if (existing != null) {
    return existing;
  }
  const created = document.createElement("div");
  created.id = CONTAINER_ID;
  document.body.append(created);
  return created;
}

/** Say what is wrong, in the page, rather than failing silently in the console. */
function renderMessage(title: string, detail: string, link?: { href: string; text: string }): void {
  const container = workbenchContainer();
  container.replaceChildren();
  const box = document.createElement("div");
  box.className = "startup-message";
  const heading = document.createElement("h1");
  heading.textContent = title;
  const paragraph = document.createElement("p");
  paragraph.textContent = detail;
  box.append(heading, paragraph);
  if (link != null) {
    const anchor = document.createElement("a");
    anchor.href = link.href;
    anchor.textContent = link.text;
    box.append(anchor);
  }
  container.append(box);
}

/**
 * The store this page will edit, or why it cannot be edited.
 *
 * A store is more than its definition (§9): it is defined *and* connected, or it
 * is a row with a reason it is not — a directory that has gone away, a git remote
 * that will not clone. The IDE asks before it boots, because a workbench over a
 * store that cannot be reached is a tree of error dialogs, and the reason the
 * admin needs is the one the store already carries.
 *
 * The record itself comes back, not merely a verdict: it carries the id the
 * operation endpoints are addressed by and whether the store is a git working
 * copy, which is what decides whether there is a Source Control view.
 */
async function openableStore(store: string): Promise<FileStoreSummary | { reason: string }> {
  const stores = await api.listFileStores();
  const found = stores.find((candidate) => candidate.name === store);
  if (found == null) {
    return {
      reason: `There is no file store named ${store}. It may have been renamed or deleted.`,
    };
  }
  if (!found.connected) {
    return { reason: found.error ?? `The file store ${store} is defined but not connected.` };
  }
  return found;
}

const store = requestedStore();
if (store == null) {
  renderMessage(
    "No file store named",
    "This page edits one file store, named by the `store` query parameter — for example /ide/?store=app-source.",
    { href: "/", text: "Back to the admin UI" },
  );
} else {
  document.title = `${store} — Saltcorn IDE`;
  try {
    const found = await openableStore(store);
    if ("reason" in found) {
      renderMessage(`Cannot edit ${store}`, found.reason, {
        href: "/",
        text: "Back to the admin UI",
      });
    } else {
      const { bootWorkbench } = await import("./workbench");
      await bootWorkbench(found, workbenchContainer(), requestedPath(window.location.href));
    }
  } catch (err) {
    // A session that expired between the page load and this call: the admin UI
    // is where logging in happens, so go there rather than explain it.
    if (errorStatus(err) === 401) {
      window.location.href = "/";
    } else {
      renderMessage(
        "The editor failed to start",
        err instanceof Error ? err.message : String(err),
        { href: "/", text: "Back to the admin UI" },
      );
      throw err;
    }
  }
}
