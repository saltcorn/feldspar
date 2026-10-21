// External identity providers (Settings → Authentication): the model behind
// the tab's provider list and the sign-in page's "Sign in with …" buttons.
//
// The provider form itself is generic — the server declares its settings
// (`getOAuthProviderSpec`) and `SettingsFields` renders them — so what lives
// here is the little the screens add: the payload a save sends, the address a
// provider must be told to send people back to, a preset's name filled in for
// the admin, and the error a failed sign-in comes back to the page with.

import { buildConfig, type FieldSpec } from "./settings";

/** The query parameter a failed sign-in comes back under. */
export const OAUTH_ERROR_PARAM = "oauth_error";

/** One provider as the sign-in page offers it. */
export type SignInProvider = { name: string; label: string; url: string };

/** The well-known providers' own names, for a new provider's button label. */
const PRESET_LABELS: Record<string, string> = {
  google: "Google",
  microsoft: "Microsoft",
  github: "GitHub",
  gitlab: "GitLab",
};

/** The body a create or update sends. */
export function providerPayload(
  spec: FieldSpec[],
  values: Record<string, string>,
): { config: Record<string, unknown> } {
  return { config: buildConfig(spec, values) };
}

/** The address to register with a provider as its redirect (callback) URL. */
export function callbackUrl(origin: string, path: string): string {
  return `${origin.replace(/\/+$/, "")}${path}`;
}

/** `values` after the admin picked `preset`: a blank name and label are
 * filled with the preset's, so a Google provider needs only its keys. */
export function withPreset(
  values: Record<string, string>,
  preset: string,
): Record<string, string> {
  const next: Record<string, string> = { ...values, preset };
  if (preset === "custom") return next;
  if (!next.name?.trim()) next.name = preset;
  if (!next.label?.trim()) next.label = PRESET_LABELS[preset] ?? preset;
  return next;
}

/** The error a failed sign-in came back with, from the page's query string. */
export function oauthErrorFrom(search: string): string | null {
  const value = new URLSearchParams(search).get(OAUTH_ERROR_PARAM);
  return value && value.trim() ? value : null;
}

/** The address that starts signing in with `provider` and comes back to
 * `returnTo`. */
export function startUrl(provider: SignInProvider, returnTo: string): string {
  const sep = provider.url.includes("?") ? "&" : "?";
  return `${provider.url}${sep}return_to=${encodeURIComponent(returnTo)}`;
}

/** Read the providers answer, keeping only well-formed entries. */
export function readSignInProviders(raw: unknown): SignInProvider[] {
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (p): p is SignInProvider =>
      !!p &&
      typeof p === "object" &&
      typeof (p as SignInProvider).name === "string" &&
      typeof (p as SignInProvider).label === "string" &&
      typeof (p as SignInProvider).url === "string",
  );
}
