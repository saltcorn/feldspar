// The Development tab's other half: the credentials an external coding agent
// administers this installation with (§13.6).
//
// Kept out of the component for the reason `modules.ts` and `backup.ts` are —
// the arithmetic of the panel is testable without a browser, and the component
// is then only flow. What that arithmetic is, and why each piece of it is here
// rather than inlined:
//
// - **The grant checkboxes are the copilot's own**, fetched from
//   `listAgentTraits` rather than written out here. One vocabulary for "what may
//   this agent do to my installation?" is the whole of §13.6's third decision,
//   and a second copy of six labels in TypeScript is how a vocabulary becomes
//   two. The fallback below exists for the server that has no `admin_copilot`
//   trait registered at all, which is the only case where there is nothing to
//   copy.
// - **The `claude mcp add` line is built here**, from this server's own origin
//   and the token that is still on screen. The setup step that gets typed wrong
//   is the one that is retyped from two places.
// - **A disabled server says so.** Offering to mint a credential that the route
//   will not look at is offering to spend somebody's afternoon.

import type {
  CreateApiTokenRequest,
  ListAgentTraitsResponse,
  ListApiTokensResponse,
} from "./client";
import type { FieldSpec } from "./settings";

/** One stored token, as `listApiTokens` describes it — never its hash and never
 * its plaintext, because the table holds neither in a readable form. */
export type ApiToken = ListApiTokensResponse[number];

/** The `_fd_config` key that decides whether `POST /mcp` exists
 * (`sc_config::MCP_ENABLED`).
 *
 * Spelled here rather than imported because it is a *protocol* constant — it
 * travels in the settings JSON — which is the arrangement every other key name
 * in this SPA lives under. */
export const MCP_ENABLED = "mcp_enabled";

/** The agent trait whose configuration form these grants are (§11.3). */
export const ADMIN_COPILOT = "admin_copilot";

/** The eight flags a token carries, in the order an admin ticks them: four
 * grants — what it may *do* — then four areas, which say what it may do it *to*.
 * A fresh token has all eight (`sc_api::mcp::token_flags_from_submitted`). */
export const GRANT_KEYS = [
  "allow_create",
  "allow_edit",
  "allow_drop",
  "allow_access_changes",
  "allow_triggers",
  "allow_applications",
  "allow_modules",
  "allow_history",
] as const;

/** The key a token stores to hold every grant — the eight above and any a later
 * version adds (`sc_api::mcp::ALLOW_ALL`). A token's own key; the copilot has no
 * such checkbox. */
export const ALLOW_ALL = "allow_all";

/** What the checkboxes fall back to when this server registers no
 * `admin_copilot` trait, so there is no label to reuse.
 *
 * Deliberately terse, and deliberately not a second set of the copilot's
 * sentences: a short true label that is obviously the key's own word cannot
 * drift into disagreeing with a longer one it is not pretending to copy. The
 * `default`s here are the copilot's; a token's mint form ignores them and starts
 * with every box ticked (`defaultGrants`). */
export const FALLBACK_GRANT_FIELDS: FieldSpec[] = [
  { name: "allow_create", label: "May create", default: true },
  { name: "allow_edit", label: "May change what is there", default: true },
  { name: "allow_drop", label: "May drop and delete", default: false },
  { name: "allow_access_changes", label: "May change access rules", default: false },
  { name: "allow_triggers", label: "May work on triggers", default: true },
  { name: "allow_applications", label: "May work on applications", default: true },
  {
    name: "allow_modules",
    label: "May list and install modules (npm and PyPI packages run with the server's privileges)",
    default: false,
  },
  {
    name: "allow_history",
    label: "May read row history (old values of rows, deleted rows and who changed them)",
    default: false,
  },
].map((field) => ({
  ...field,
  type: "bool",
  required: false,
  options: [],
  multiline: false,
}));

/** The grant checkboxes, taken from the copilot's own configuration form.
 *
 * The eight of `GRANT_KEYS`, in that order, each with the label and default the
 * server declared for it. A key the trait does not declare falls back to its own
 * entry above rather than disappearing: a token missing a checkbox would be a
 * token minted with a flag nobody was asked about. */
export function grantFields(traits: ListAgentTraitsResponse): FieldSpec[] {
  const spec = traits.find((trait) => trait.name === ADMIN_COPILOT)?.config_spec ?? [];
  return FALLBACK_GRANT_FIELDS.map(
    (fallback) => spec.find((field) => field.name === fallback.name) ?? fallback,
  );
}

/** Which boxes a fresh mint form starts with ticked: all of them, whatever the
 * copilot declares as its own defaults. A token is minted on purpose for an agent
 * the admin is handing the installation to, and the server grants an absent flag
 * too (`sc_api::mcp::token_flags_from_submitted`); unticking is one click. */
export function defaultGrants(fields: FieldSpec[]): Record<string, boolean> {
  const grants: Record<string, boolean> = {};
  for (const field of fields) grants[field.name] = true;
  return grants;
}

/** What an admin fills in to mint one. */
export type MintForm = {
  label: string;
  grants: Record<string, boolean>;
  /** The "all grants" toggle: stored on the token as `allow_all`, so it also
   * holds every grant a later version adds. An unticked box beside it is still
   * an exception. */
  all: boolean;
  /** Days, as typed. Empty means a token that does not lapse. */
  expiresInDays: string;
};

/** A blank mint form under a set of checkboxes. Ninety days rather than none:
 * the expiry is the only bound a credential has that does not need somebody to
 * remember it. */
export function emptyMint(fields: FieldSpec[]): MintForm {
  return { label: "", grants: defaultGrants(fields), all: true, expiresInDays: "90" };
}

/** Why this form cannot be submitted yet, in a sentence, or `null`.
 *
 * The label is required *here* rather than only at the server because it is the
 * thing the audit line names and the thing the Revoke button is read off: a
 * token called "" is one an admin cannot tell from another one. */
export function mintProblem(form: MintForm): string | null {
  if (form.label.trim() === "") return "Give the token a label — it is what the log names.";
  if (form.expiresInDays.trim() !== "") {
    const days = Number(form.expiresInDays);
    if (!Number.isInteger(days) || days < 1) return "An expiry is a whole number of days.";
  }
  return null;
}

/** What Mint sends: the label, all eight flags explicitly, and an expiry in days
 * or nothing at all.
 *
 * The flags are sent in full rather than only the ticked ones. The server writes
 * every flag explicitly for the same reason (§13.6): a stored `grants` is the
 * record of what an admin agreed to, and a missing key that reads as a default
 * today reads as a different default the day a default changes. */
export function mintRequest(form: MintForm, fields: FieldSpec[]): CreateApiTokenRequest {
  const grants: Record<string, boolean> = {};
  for (const field of fields) grants[field.name] = form.grants[field.name] === true;
  grants[ALLOW_ALL] = form.all;
  const days = form.expiresInDays.trim();
  return {
    label: form.label.trim(),
    grants,
    ...(days === "" ? {} : { expires_in_days: Number(days) }),
  };
}

/** This server's MCP endpoint, from the page's own origin.
 *
 * The origin is the browser's, which is the one address known to reach this
 * server — a configured `base_url` would be a second thing to keep right, and
 * the admin is standing at the URL that works. */
export function mcpUrl(origin: string): string {
  return `${origin.replace(/\/+$/, "")}/mcp`;
}

/** What to type where the token goes once it is off the screen. Recognisably
 * the prefix a real one carries, so the line is obviously incomplete rather than
 * subtly wrong. */
export const TOKEN_PLACEHOLDER = "fspk_YOUR_TOKEN";

/** The one line that registers this server with Claude Code, with the token in
 * it while there still is one.
 *
 * `--transport http` because §13.6 serves streamable HTTP and no stdio, and the
 * header because bearer is the only credential this route accepts — a cookie on
 * it is ignored, not honoured. */
export function claudeMcpAddLine(
  origin: string,
  secret: string | null,
  name = "feldspar",
): string {
  const token = secret ?? TOKEN_PLACEHOLDER;
  return `claude mcp add --transport http ${name} ${mcpUrl(origin)} --header "Authorization: Bearer ${token}"`;
}

/** How a token's state reads in the list. `live` is the server's own arithmetic
 * — neither revoked nor lapsed — so nothing here recomputes the clock; what is
 * decided here is only which of the two ways of not being live this is, because
 * they are different things to have happened. */
export function tokenBadge(token: ApiToken): { label: string; tone: string } {
  if (token.revoked_at) return { label: "Revoked", tone: "bg-red-lt" };
  if (!token.live) return { label: "Expired", tone: "bg-yellow-lt" };
  return { label: "Active", tone: "bg-green-lt" };
}

/** The flags this token was minted with, as the keys that are on.
 *
 * `grants` arrives as `unknown` because the column is JSON; a row written by
 * this server has all eight, and one that somehow has fewer reads as "not granted"
 * rather than as an error, which is what the server would also decide. */
export function grantedKeys(token: ApiToken): string[] {
  const grants = token.grants;
  if (!grants || typeof grants !== "object") return [];
  const bag = grants as Record<string, unknown>;
  // `allow_all` holds a flag the row does not mention (`sc_api::mcp::token_flags`).
  const all = bag[ALLOW_ALL] === true;
  return GRANT_KEYS.filter((key) => bag[key] === true || (all && !(key in bag)));
}

/** Whether this token was minted with "all grants", so it also holds every grant
 * a later version adds. */
export function holdsAll(token: ApiToken): boolean {
  const grants = token.grants;
  return !!grants && typeof grants === "object" && (grants as Record<string, unknown>)[ALLOW_ALL] === true;
}

/** A flag's own word, for a badge: the key without its `allow_` and without its
 * underscores. The checkboxes carry the copilot's sentences; a list of six of
 * those would be a paragraph per row. */
export function shortGrantLabel(key: string): string {
  return key.replace(/^allow_/, "").replace(/_/g, " ");
}

/** A timestamp as the list shows it, or the dash that means there is none —
 * which for `last_used_at` means a credential that was minted and never used,
 * and is worth seeing. */
export function when(value: string | null | undefined): string {
  if (!value) return "—";
  const at = new Date(value);
  return Number.isNaN(at.getTime()) ? value : at.toLocaleString();
}

/** Whether `POST /mcp` is served, read from the settings as they are **stored**.
 *
 * Stored rather than as-typed: a token minted against a ticked-but-unsaved
 * checkbox is a token that does not work, and this panel's job is to not offer
 * that. */
export function mcpEnabled(stored: Record<string, string>): boolean {
  return stored[MCP_ENABLED] === "true";
}
