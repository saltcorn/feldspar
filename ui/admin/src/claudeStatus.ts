// What Settings → Development says about Claude Code (UX1.6): which `claude`
// the IDE's terminal would run, how it was found, and what to do when there is
// none. `screens/ClaudeCodeStatus.tsx` draws it; `claudeStatus.test.ts` pins it.

import type { GetClaudeCodeStatusResponse } from "./client";
import { format, type Translator } from "./i18n";

type T = Translator["t"];

/** The setting holding the executable's full path (`sc_config::CLAUDE_CODE_PATH`). */
export const CLAUDE_CODE_PATH = "claude_code_path";
/** The switch the terminal, and so this panel, depends on. */
export const IDE_TERMINAL_ENABLED = "ide_terminal_enabled";

/** Whether the form has Claude Code in the IDE ticked. */
export function terminalTicked(values: Record<string, string>): boolean {
  return values[IDE_TERMINAL_ENABLED] === "true";
}

/** How the server found the executable, in a sentence's worth of words. */
export function sourceLabel(source: string | null | undefined, t: T = format): string | null {
  switch (source) {
    case "option":
      return t("named by the server's --claude-cli option");
    case "setting":
      return t("named by the setting above");
    case "path":
      return t("found on the server's PATH");
    case "usual":
      return t("found in a usual install location");
    default:
      return null;
  }
}

/** The badge: ready, found but not answering, or not found. */
export function statusBadge(
  status: Pick<GetClaudeCodeStatusResponse, "path" | "version">,
  t: T = format,
): { tone: string; label: string } {
  if (status.path && status.version) return { tone: "bg-green-lt", label: t("Ready") };
  if (status.path) return { tone: "bg-yellow-lt", label: t("Found, but it did not answer") };
  return { tone: "bg-red-lt", label: t("Not found") };
}

/** A key that changes when a save changes what the status depends on, so the
 * panel reads it again rather than showing the answer from before the save. */
export function statusKey(stored: Record<string, string>): string {
  return `${stored[IDE_TERMINAL_ENABLED] ?? ""}|${stored[CLAUDE_CODE_PATH] ?? ""}`;
}
