import * as vscode from "vscode";
import "vscode/localExtensionHost";
import {
  initialize as initializeVscodeApi,
  LogLevel,
  type IEditorOverrideServices,
  type IWorkbenchConstructionOptions,
} from "@codingame/monaco-vscode-api";
import type { EnvironmentOverride } from "@codingame/monaco-vscode-api/workbench";
import getWorkbenchServiceOverride from "@codingame/monaco-vscode-workbench-service-override";
import getConfigurationServiceOverride, {
  initUserConfiguration,
} from "@codingame/monaco-vscode-configuration-service-override";
import getKeybindingsServiceOverride, {
  initUserKeybindings,
} from "@codingame/monaco-vscode-keybindings-service-override";
import getDialogsServiceOverride from "@codingame/monaco-vscode-dialogs-service-override";
import { getDecorationsServiceOverride } from "./decorations";
import getExplorerServiceOverride from "@codingame/monaco-vscode-explorer-service-override";
import getExtensionServiceOverride from "@codingame/monaco-vscode-extensions-service-override";
import getLanguagesServiceOverride from "@codingame/monaco-vscode-languages-service-override";
import getLifecycleServiceOverride from "@codingame/monaco-vscode-lifecycle-service-override";
import getMarkersServiceOverride from "@codingame/monaco-vscode-markers-service-override";
import getModelServiceOverride from "@codingame/monaco-vscode-model-service-override";
import getNotificationServiceOverride from "@codingame/monaco-vscode-notifications-service-override";
import getOutputServiceOverride from "@codingame/monaco-vscode-output-service-override";
import getPreferencesServiceOverride from "@codingame/monaco-vscode-preferences-service-override";
import getQuickAccessServiceOverride from "@codingame/monaco-vscode-quickaccess-service-override";
import getScmServiceOverride from "@codingame/monaco-vscode-scm-service-override";
import getSearchServiceOverride from "@codingame/monaco-vscode-search-service-override";
import getSecretStorageServiceOverride from "@codingame/monaco-vscode-secret-storage-service-override";
import getStatusBarServiceOverride from "@codingame/monaco-vscode-view-status-bar-service-override";
import getStorageServiceOverride from "@codingame/monaco-vscode-storage-service-override";
import getTextmateServiceOverride from "@codingame/monaco-vscode-textmate-service-override";
import getThemeServiceOverride from "@codingame/monaco-vscode-theme-service-override";
import getTitleBarServiceOverride from "@codingame/monaco-vscode-view-title-bar-service-override";
import getWorkingCopyServiceOverride from "@codingame/monaco-vscode-working-copy-service-override";

// The grammars and the theme, as VS Code's own built-in extensions. These are
// *declarative* extensions — TextMate grammars and language configuration — so
// they are what makes a React project's files look like code. Semantics (a
// language server) is phase 4; nothing here runs tsserver.
import "@codingame/monaco-vscode-theme-defaults-default-extension";
import "@codingame/monaco-vscode-typescript-basics-default-extension";
import "@codingame/monaco-vscode-javascript-default-extension";
import "@codingame/monaco-vscode-json-default-extension";
import "@codingame/monaco-vscode-css-default-extension";
import "@codingame/monaco-vscode-html-default-extension";
import "@codingame/monaco-vscode-markdown-basics-default-extension";

import defaultConfiguration from "./user/configuration.json?raw";
import defaultKeybindings from "./user/keybindings.json?raw";
import { api } from "./api";
import { activateAgentChat, declareAgentChat } from "./chat";
import { codingAgentsForStore } from "./codingAgents";
import {
  activateSaltcornExtension,
  declareSaltcornExtension,
} from "./extension";
import { storeGit, type FileStoreSummary } from "./git";
import { configureWorkers } from "./workers";
import { registerStoreSearch } from "./searchProvider";
import { toUriPath } from "./storeFiles";
import { registerStoreFilesystem, storeFolderUri } from "./workspace";

/**
 * The services this IDE runs on, plus `chat`'s when the store has an agent to
 * chat with.
 *
 * `getWorkbenchServiceOverride` is the whole point (design §12.1): it renders VS
 * Code's real workbench — activity bar, explorer tree, editor tabs, panels, status
 * bar — rather than an editor we have to surround with our own furniture. The rest
 * are the services that workbench then expects to find, and the list is
 * deliberately shorter than upstream's demo: no debug, no notebooks, no terminal
 * (a shell on the server is a decision of its own), no extension gallery
 * (installing extensions is out of scope for this milestone).
 *
 * **Chat is conditional**, which source control deliberately is not. The
 * difference is what the empty state costs: a workbench with the SCM service and
 * no provider shows "no source control providers", which is true and harmless,
 * while a chat view with no participant and no model is a composer that accepts
 * a question and then fails on it. A store with no agent scoped to it (§11.2)
 * therefore gets the workbench it had before this existed — no view, no panel,
 * nothing to explain — and does not download the five megabytes that would have
 * drawn it.
 */
function services(chat: ChatServices | null): IEditorOverrideServices {
  return {
    ...getWorkbenchServiceOverride(),
    ...getExplorerServiceOverride(),
    ...getTitleBarServiceOverride(),
    ...getStatusBarServiceOverride(),
    ...getQuickAccessServiceOverride({
      isKeybindingConfigurationVisible: () => true,
      shouldUseGlobalPicker: () => true,
    }),
    ...getSearchServiceOverride(),
    ...getMarkersServiceOverride(),
    ...getOutputServiceOverride(),
    ...getPreferencesServiceOverride(),
    ...getConfigurationServiceOverride(),
    ...getKeybindingsServiceOverride(),
    ...getStorageServiceOverride({
      fallbackOverride: {
        // There is no account to sign into here in any configuration: this
        // workbench is reached by being signed into Saltcorn.
        "workbench.activity.showAccounts": false,
        // The chat's setup state, when there is a chat; see `loadChatServices`.
        ...(chat?.storage ?? {}),
      },
    }),
    ...getSecretStorageServiceOverride(),
    ...getModelServiceOverride(),
    ...getLanguagesServiceOverride(),
    ...getTextmateServiceOverride(),
    ...getThemeServiceOverride(),
    ...getNotificationServiceOverride(),
    ...getDialogsServiceOverride(),
    ...getLifecycleServiceOverride(),
    ...getWorkingCopyServiceOverride(),
    ...getExtensionServiceOverride({ enableWorkerExtensionHost: true }),
    // Source control. What this adds is the ability to *host* a provider: the
    // Source Control viewlet and its icon are the workbench's own and are there
    // either way — leaving this out was tried, and all it changes is that the view
    // cannot work. Whether there is a provider to show is `sourceControl.ts`'s
    // question, and for a store that is not a git working copy the answer is no,
    // which VS Code renders as its own "no source control providers" empty state.
    ...getScmServiceOverride(),
    // …and the service that draws the letter at the end of each of its rows,
    // which the workbench otherwise stubs out (see `decorations.ts`).
    ...getDecorationsServiceOverride(),
    ...(chat?.services ?? {}),
  };
}

/** The chat's service overrides, and the storage it must be seeded with. */
interface ChatServices {
  readonly services: IEditorOverrideServices;
  readonly storage: Record<string, unknown>;
}

/**
 * Load the three overrides the chat view needs, and say what to seed storage
 * with.
 *
 * **Dynamically imported, and that is load-bearing rather than tidy.** These
 * packages register their contributions — views, commands, the chat viewlet — as
 * a side effect of being *imported*, not of the override function being called,
 * so a static import puts the chat view in every workbench and only the
 * `getChatServiceOverride()` call would be conditional. It is also the larger
 * half of this bundle, which a store holding assets has no use for.
 *
 * Two of the three were found the way these things are found — chat failed, and
 * the *error* named the service. Both throw rather than degrade, so leaving
 * either out is a chat that takes a question, answers nothing and says nothing.
 */
async function loadChatServices(): Promise<ChatServices> {
  const [chat, mcp, accessibility] = await Promise.all([
    import("@codingame/monaco-vscode-chat-service-override"),
    // Chat asks the MCP service to start whatever servers are configured before
    // it hands the request to a participant, and an unregistered service throws
    // there: the request fails before the participant sees it. This is
    // therefore not a decision to support MCP in the IDE — nothing here
    // configures a server, so it starts none.
    import("@codingame/monaco-vscode-mcp-service-override"),
    // The chat view builds each answer's accessible label as it renders it, and
    // an unregistered accessibility service throws *there* — so the agent's
    // answer arrives, the row fails to draw, and the panel stays empty. It is a
    // screen reader's service, and it is load-bearing for everyone.
    import("@codingame/monaco-vscode-accessibility-service-override"),
  ]);
  return {
    services: {
      ...accessibility.default(),
      ...mcp.default(),
      // `defaultAccount` is the same fiction as the storage seed below: the
      // workbench asks whether chat is enabled for the signed-in account, and
      // the answer is not "Copilot says yes" but "this is not Copilot" — the
      // agent behind the panel is this deployment's own, reached over the
      // provider credentials the agent already carries (§11.2).
      ...chat.default({
        defaultAccount: {
          sessionId: "saltcorn",
          accountName: "saltcorn",
          enterprise: true,
          authenticationProvider: {
            id: "saltcorn",
            name: "Saltcorn",
            enterprise: true,
          },
          entitlementsData: {
            access_type_sku: "saltcorn",
            assigned_date: "",
            can_signup_for_limited: false,
            copilot_plan: "enterprise",
            organization_login_list: [],
            analytics_tracking_id: "",
            chat_enabled: true,
          },
        },
      }),
    },
    // VS Code keeps the chat's *setup* state — signed in, entitled, installed —
    // in its own storage, and offers no way to say "this deployment brings its
    // own model" other than seeding it. Without this the chat view is Copilot's
    // sign-up flow: a sign-up flow for a product this installation is not using.
    storage: {
      "chat.setupContext": {
        entitlement: chat.ChatEntitlement.Enterprise,
        registered: true,
        completed: true,
        installed: true,
      },
    },
  };
}

/**
 * Otherwise VS Code takes the first workspace folder for the user's home
 * directory, which makes find-in-files report paths relative to the wrong root.
 */
const environment: EnvironmentOverride = {
  userHome: vscode.Uri.file("/"),
};

function constructionOptions(store: string): IWorkbenchConstructionOptions {
  return {
    workspaceProvider: {
      trusted: true,
      workspace: { folderUri: storeFolderUri(store) },
      // A page edits exactly one store: VS Code cannot be re-initialized without
      // a page load (§12.1), so "open another workspace" is refused rather than
      // half-implemented. Switching stores is a navigation.
      async open() {
        return false;
      },
    },
    developmentOptions: { logLevel: LogLevel.Info },
    windowIndicator: {
      label: `$(database) ${store}`,
      tooltip: `Saltcorn file store: ${store}`,
      command: "",
    },
    configurationDefaults: {
      "window.title": `${store}\${separator}\${dirty}\${activeEditorShort}`,
    },
    productConfiguration: {
      nameShort: "Saltcorn",
      nameLong: "Saltcorn IDE",
    },
  };
}

/**
 * Boot the workbench for `summary`'s store into `container`.
 *
 * The whole store record rather than its name: source control is addressed by
 * the store's **id** and offered only for a git working copy, and both facts are
 * in the listing the page has already read to decide it can open at all.
 */
export async function bootWorkbench(
  summary: FileStoreSummary,
  container: HTMLElement,
  /** A file to open once the workbench is up — what "Open in IDE" on a model's
   * program asks for. */
  open: string | null = null,
): Promise<void> {
  const store = summary.name;
  configureWorkers();
  // Before `initialize`, so the theme is right on the first frame rather than
  // after a flash of the default one.
  await Promise.all([
    initUserConfiguration(defaultConfiguration),
    initUserKeybindings(defaultKeybindings),
  ]);
  const { files, provider } = registerStoreFilesystem(store);
  // Declared before `initialize` so it is one of the workbench's built-in
  // extensions, and activated after it so the API it hands out has services to
  // talk to. Both halves matter; `extension.ts` says what goes wrong otherwise.
  const extension = declareSaltcornExtension();
  // The same rule, and the reason this listing is read *here* rather than by the
  // chat: what a manifest contributes is fixed when the extension is declared,
  // so which participants exist has to be known before the workbench starts. A
  // failure to read it costs the chat panel and nothing else — an IDE that will
  // not open because the agent listing was slow would be a poor trade.
  const agents = await storeAgents(store);
  const chat = declareAgentChat(agents);
  const chatServices = chat == null ? null : await loadChatServices();
  const git = storeGit(summary, api);
  await initializeVscodeApi(
    services(chatServices),
    container,
    constructionOptions(store),
    environment,
  );
  // After `initialize`, because it registers with a service that must exist by
  // then: find-in-files stops walking the tree a directory at a time and asks
  // the store instead (§12.1).
  await registerStoreSearch(store, api);
  const saltcorn = await activateSaltcornExtension(
    extension,
    files,
    provider,
    git,
  );
  if (chat != null) {
    await activateAgentChat(chat, agents, store, {
      files,
      provider,
      refreshSourceControl: saltcorn.refreshSourceControl,
    });
  }
  if (open != null) {
    // A file that is not there is said by the editor itself; the workbench is
    // still the store's.
    await vscode.window
      .showTextDocument(vscode.Uri.file(toUriPath(store, open)))
      .then(undefined, (err: unknown) => console.warn(`[saltcorn] could not open ${open}`, err));
  }
}

/**
 * The agents whose `coding` trait is scoped to `store`.
 *
 * Two listings, because an agent knows which *store* it works on and the chat
 * wants to say which *application* that is — and both are already served
 * (§13.1). Neither is fatal: an installation with agents switched off answers
 * one of them with a failure, and the right consequence of that is a workbench
 * with no chat, not a workbench that did not open.
 */
async function storeAgents(store: string) {
  try {
    const [agents, applications] = await Promise.all([
      api.listAgents(),
      api.listApplications(),
    ]);
    return codingAgentsForStore(agents, applications, store);
  } catch (err) {
    console.warn(
      "[saltcorn] the agent listing could not be read; chat is unavailable",
      err,
    );
    return [];
  }
}
