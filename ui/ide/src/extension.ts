/**
 * Saltcorn's own contributions to the workbench, as an extension that is never
 * packaged (design §12.1, decision 2).
 *
 * The workbench runs in this page's JavaScript context, so `registerExtension`
 * takes a manifest **object** and `setAsDefaultApi` makes the `vscode` import in
 * these modules that extension's API. What would be a `.vsix` elsewhere — a build
 * command, a formatter — is a manifest literal and two ordinary function calls.
 */

import {
  ExtensionHostKind,
  registerExtension,
} from "@codingame/monaco-vscode-api/extensions";
import type {
  IExtensionManifest,
  RegisterLocalProcessExtensionResult,
} from "@codingame/monaco-vscode-api/extensions";

import { BUILD_COMMAND, BUILD_ON_SAVE_COMMAND, registerBuildCommand } from "./build";
import { CLAUDE_COMMAND, CLAUDE_CONTEXT } from "./claudeTerminal";
import type { StoreFileSystemProvider } from "./fileSystemProvider";
import { registerPrettierFormatter } from "./formatter";
import { StoreGit } from "./git";
import { registerLanguageClient } from "./languageClient";
import {
  CHECKOUT_COMMAND,
  COMMIT_COMMAND,
  GIT_STORE_CONTEXT,
  GROUP_CONTEXT,
  PULL_COMMAND,
  PUSH_COMMAND,
  REFRESH_COMMAND,
  STAGE_ALL_COMMAND,
  STAGE_COMMAND,
  UNSTAGE_ALL_COMMAND,
  UNSTAGE_COMMAND,
  registerSourceControl,
} from "./sourceControl";
import type { StoreFiles } from "./storeFiles";

/**
 * The manifest. `contributes.commands` is what puts Build in the command palette
 * (with its category, so "Saltcorn: Build Application" finds it); the button in
 * the status bar is created by the command's own registration.
 */
const MANIFEST: IExtensionManifest = {
  name: "saltcorn",
  displayName: "Saltcorn",
  publisher: "saltcorn",
  version: "1.0.0",
  engines: { vscode: "*" },
  // No `main`: the extension's code is this bundle, already loaded. The wildcard
  // event is what makes the contributed command available from the first frame.
  activationEvents: ["*"],
  contributes: {
    commands: [
      {
        command: BUILD_COMMAND,
        title: "Build Application",
        category: "Saltcorn",
        icon: "$(tools)",
      },
      {
        command: BUILD_ON_SAVE_COMMAND,
        title: "Toggle Build on Save",
        category: "Saltcorn",
        icon: "$(sync)",
      },
      {
        command: CLAUDE_COMMAND,
        title: "Open Claude Code",
        category: "Saltcorn",
        icon: "$(sparkle)",
      },
      {
        command: COMMIT_COMMAND,
        title: "Commit",
        category: "Git",
        icon: "$(check)",
      },
      {
        command: PULL_COMMAND,
        title: "Pull",
        category: "Git",
        icon: "$(cloud-download)",
      },
      {
        command: PUSH_COMMAND,
        title: "Push",
        category: "Git",
        icon: "$(cloud-upload)",
      },
      {
        command: CHECKOUT_COMMAND,
        title: "Switch Branch…",
        category: "Git",
        icon: "$(git-branch)",
      },
      {
        command: REFRESH_COMMAND,
        title: "Refresh",
        category: "Git",
        icon: "$(refresh)",
      },
      // The index's four. `$(add)`/`$(remove)` are the icons VS Code's own git
      // view uses for them, which is the whole point of having them here.
      {
        command: STAGE_COMMAND,
        title: "Stage Changes",
        category: "Git",
        icon: "$(add)",
      },
      {
        command: UNSTAGE_COMMAND,
        title: "Unstage Changes",
        category: "Git",
        icon: "$(remove)",
      },
      {
        command: STAGE_ALL_COMMAND,
        title: "Stage All Changes",
        category: "Git",
        icon: "$(add)",
      },
      {
        command: UNSTAGE_ALL_COMMAND,
        title: "Unstage All Changes",
        category: "Git",
        icon: "$(remove)",
      },
    ],
    // The letters' colours, with VS Code's own ids and VS Code's own defaults.
    // The bundled themes define them, so this changes nothing under those — it is
    // here so that a theme which does not still draws a `U` in green rather than
    // in the foreground colour, which would make the letters the only thing
    // distinguishing one kind of change from another.
    colors: [
      colour("gitDecoration.addedResourceForeground", "#81b88b", "#587c0c"),
      colour("gitDecoration.modifiedResourceForeground", "#E2C08D", "#895503"),
      colour("gitDecoration.deletedResourceForeground", "#c74e39", "#ad0707"),
      colour("gitDecoration.untrackedResourceForeground", "#73C991", "#007100"),
      colour("gitDecoration.conflictingResourceForeground", "#e4676b", "#ad0707"),
      colour("gitDecoration.stageModifiedResourceForeground", "#E2C08D", "#895503"),
      colour("gitDecoration.stageDeletedResourceForeground", "#c74e39", "#ad0707"),
    ],
    // The Source Control view's title bar, and the palette. Both are gated on
    // the same context key, which is set only for a store that is a git working
    // copy: a local-directory store must not be offered a Pull that can only
    // fail. `navigation` is the group that renders as icons rather than as an
    // overflow menu.
    menus: {
      // A row's own `+` or `−`, and a group's. `inline` is the group that renders
      // as an icon on the row; `1_modification` is the same command in the
      // right-click menu, where a name is shown rather than an icon.
      "scm/resourceState/context": [
        {
          command: STAGE_COMMAND,
          group: "inline",
          when: `scmResourceState == ${GROUP_CONTEXT.unstaged}`,
        },
        {
          command: STAGE_COMMAND,
          group: "1_modification",
          when: `scmResourceState == ${GROUP_CONTEXT.unstaged}`,
        },
        // A conflicted file is staged to mark it resolved, which is what `git
        // add` means on one — so the same button, for the same reason VS Code
        // offers it there.
        {
          command: STAGE_COMMAND,
          group: "inline",
          when: `scmResourceState == ${GROUP_CONTEXT.merge}`,
        },
        {
          command: STAGE_COMMAND,
          group: "1_modification",
          when: `scmResourceState == ${GROUP_CONTEXT.merge}`,
        },
        {
          command: UNSTAGE_COMMAND,
          group: "inline",
          when: `scmResourceState == ${GROUP_CONTEXT.staged}`,
        },
        {
          command: UNSTAGE_COMMAND,
          group: "1_modification",
          when: `scmResourceState == ${GROUP_CONTEXT.staged}`,
        },
      ],
      "scm/resourceGroup/context": [
        {
          command: STAGE_ALL_COMMAND,
          group: "inline",
          when: `scmResourceGroupState == ${GROUP_CONTEXT.unstaged}`,
        },
        {
          command: STAGE_ALL_COMMAND,
          group: "1_modification",
          when: `scmResourceGroupState == ${GROUP_CONTEXT.unstaged}`,
        },
        {
          command: STAGE_ALL_COMMAND,
          group: "inline",
          when: `scmResourceGroupState == ${GROUP_CONTEXT.merge}`,
        },
        {
          command: UNSTAGE_ALL_COMMAND,
          group: "inline",
          when: `scmResourceGroupState == ${GROUP_CONTEXT.staged}`,
        },
        {
          command: UNSTAGE_ALL_COMMAND,
          group: "1_modification",
          when: `scmResourceGroupState == ${GROUP_CONTEXT.staged}`,
        },
      ],
      "scm/title": [
        {
          command: COMMIT_COMMAND,
          group: "navigation",
          when: `${GIT_STORE_CONTEXT}`,
        },
        {
          command: REFRESH_COMMAND,
          group: "navigation",
          when: `${GIT_STORE_CONTEXT}`,
        },
        {
          command: PULL_COMMAND,
          group: "navigation",
          when: `${GIT_STORE_CONTEXT}`,
        },
        {
          command: PUSH_COMMAND,
          group: "navigation",
          when: `${GIT_STORE_CONTEXT}`,
        },
        {
          command: CHECKOUT_COMMAND,
          group: "navigation",
          when: `${GIT_STORE_CONTEXT}`,
        },
      ],
      commandPalette: [
        // Registered only where an operator has turned the terminal on.
        { command: CLAUDE_COMMAND, when: CLAUDE_CONTEXT },
        { command: COMMIT_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: PULL_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: PUSH_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: CHECKOUT_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: REFRESH_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: STAGE_ALL_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        { command: UNSTAGE_ALL_COMMAND, when: `${GIT_STORE_CONTEXT}` },
        // Not the per-row pair: they act on the rows a menu handed them, and
        // chosen from the palette there are none, which is "everything" — the
        // two commands above, under a name that does not say so.
        { command: STAGE_COMMAND, when: "false" },
        { command: UNSTAGE_COMMAND, when: "false" },
      ],
    },
  },
};

/** One colour contribution, light and dark, in the shape the manifest takes. */
function colour(
  id: string,
  dark: string,
  light: string,
): { id: string; description: string; defaults: { dark: string; light: string; highContrast: string } } {
  return {
    id,
    description: `Source control decoration: ${id}`,
    defaults: { dark, light, highContrast: dark },
  };
}

/** What activating the extension leaves for the rest of the page to use. */
export interface SaltcornExtension {
  /**
   * Source control's rescan, when the store is a git working copy.
   *
   * Handed out for the same reason the Build command is given it: anything that
   * changes the source tree *without* going through an editor — a build writing
   * `dist/`, an agent writing a file it was asked to — leaves the Changes group
   * showing what was true before it ran.
   */
  readonly refreshSourceControl?: () => void;
}

/** How long the extension's API may take to arrive before that is news. */
const API_OVERDUE_MS = 15_000;

/**
 * Declare the extension. **Must be called before `initialize`.**
 *
 * That ordering is not a style choice. `registerExtension` behaves differently
 * either side of initialization: before, the extension joins the built-in set the
 * workbench registers as it starts; after, it is a *delta* applied to a running
 * workbench, which takes the extension registry's lock and waits for every
 * extension host to accept it — including the web worker host, which this bundle
 * has no extension for and which does not start. Registering afterwards therefore
 * hangs (`handleDeltaExtensions has been holding on to the lock`), the promise
 * never settles, and everything the extension contributes silently never appears.
 */
export function declareSaltcornExtension(): RegisterLocalProcessExtensionResult {
  return registerExtension(MANIFEST, ExtensionHostKind.LocalProcess);
}

/**
 * Take over the `vscode` API and register what the extension contributes.
 *
 * Called after `initialize`, because the API talks to services that must exist:
 * `setAsDefaultApi` is what makes the `vscode` import in `formatter.ts` and
 * `build.ts` resolve to this extension's own API rather than the anonymous one.
 */
export async function activateSaltcornExtension(
  extension: RegisterLocalProcessExtensionResult,
  files: StoreFiles,
  provider: StoreFileSystemProvider,
  /**
   * The store's git side, or `null` when it is not a working copy — which is
   * what decides whether there is a Source Control view at all (§12.1).
   */
  git: StoreGit | null,
): Promise<SaltcornExtension> {
  // The failure above is a *hang*, not a throw: the workbench comes up looking
  // perfectly healthy while none of this ever runs, which is what made it cost an
  // afternoon. So a wait this long says so out loud.
  const overdue = window.setTimeout(() => {
    console.error(
      "[saltcorn] the extension API has not arrived; Build and formatting are missing. " +
        "Is the extension being registered after initialize()?",
    );
  }, API_OVERDUE_MS);
  try {
    await extension.setAsDefaultApi();
  } finally {
    window.clearTimeout(overdue);
  }
  registerPrettierFormatter(files);
  // Source control first, so the Build command can be handed its refresh: a
  // build writes the generated client into the source tree and a bundle into
  // `dist/`, which is exactly the kind of change the Changes group must show.
  const sourceControl =
    git === null ? null : registerSourceControl(files, provider, git);
  registerBuildCommand(files, sourceControl?.refresh);
  // Last, and not awaited: prettier, the Build command and the grammars are the
  // capabilities every store has, and a store that cannot host a language server
  // (§12.1) must still get all of them. The client says so for itself when the
  // socket refuses it.
  registerLanguageClient(files.store);
  return { refreshSourceControl: sourceControl?.refresh };
}
