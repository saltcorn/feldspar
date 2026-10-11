// Where the admin UI is served, and following it when it moves (Settings →
// Development → Admin subdomain).
//
// Saving a new admin subdomain moves the admin UI at once — no restart — and
// orders a certificate for the new name. The screen that saved it then opens a
// dialog that waits for three things before it lets the admin go there:
//
// 1. the move itself, which the save has already done;
// 2. a certificate naming the new host (`getAdminAddress`'s `certificate`),
//    which under ACME is a minute of traffic with the CA, and under TLS off is
//    nothing at all;
// 3. the new address answering *this browser* — fetched from here, because
//    only the browser can say whether its DNS resolves the name and its trust
//    store accepts the certificate.
//
// Following it does not mean logging in again: the session cookie is
// host-only, so the old host mints a one-time handoff (`createAdminHandoff`)
// that the new host turns into a session.

import type { GetAdminAddressResponse } from "./client";
import { format, type Translator } from "./i18n";

/** The setting the dialog watches. */
export const ADMIN_SUBDOMAIN = "admin_subdomain";

/** Whether a save moved the admin UI: the stored subdomain before and after,
 * compared as the server stores it (trimmed, lower-cased, empty for none). */
export function adminMoved(before: Record<string, string>, after: Record<string, string>): boolean {
  const norm = (v: string | undefined) => (v ?? "").trim().toLowerCase();
  return norm(before[ADMIN_SUBDOMAIN]) !== norm(after[ADMIN_SUBDOMAIN]);
}

/** Where this page is, as far as building another origin goes. */
export type Here = Pick<Location, "protocol" | "host">;

/** `:3032` when the page is on a non-default port, else empty. */
function portOf(here: Here): string {
  const colon = here.host.lastIndexOf(":");
  return colon >= 0 && !here.host.endsWith("]") ? here.host.slice(colon) : "";
}

/** The origin of `host` reached the way this page was: same scheme, same port. */
export function originOf(host: string, here: Here): string {
  return `${here.protocol}//${host}${portOf(here)}`;
}

/** One line of the dialog's progress. `label` is translated, the new host
 * filled in; `detail` is the server's own sentence (the CA's error, say) or a
 * hint, shown under it. */
export type MoveStep = {
  id: "moved" | "certificate" | "reachable";
  label: string;
  state: "done" | "active" | "pending" | "failed";
  detail?: string;
};

/** The dialog's progress, from the server's report and whether the browser has
 * reached the new address yet (`null`: not tried, because the certificate is
 * not ready to try it with). */
export function moveSteps(
  address: GetAdminAddressResponse | null,
  reachable: boolean | null,
  t: Translator["t"] = format,
): MoveStep[] {
  const host = address?.admin_host ?? t("the new address");
  const certificate = address?.certificate ?? null;
  const certStep = ((): MoveStep => {
    if (!address) return { id: "certificate", label: t("Certificate"), state: "pending" };
    switch (certificate?.state) {
      case undefined:
      case "plain_http":
        return {
          id: "certificate",
          label: t("No certificate needed: this server serves plain HTTP"),
          state: "done",
        };
      case "ready":
        return {
          id: "certificate",
          label: t("Certificate for {host} is ready", { host }),
          state: "done",
        };
      case "ordering":
        return {
          id: "certificate",
          label: t("Obtaining a certificate for {host}…", { host }),
          state: "active",
          detail: certificate.message ?? undefined,
        };
      default:
        return {
          id: "certificate",
          label: t("No certificate covers {host}", { host }),
          state: "failed",
          detail: certificate?.message ?? undefined,
        };
    }
  })();
  const reachStep: MoveStep =
    certStep.state !== "done"
      ? {
          id: "reachable",
          label: t("{host} answers this browser", { host }),
          state: "pending",
        }
      : reachable
        ? {
            id: "reachable",
            label: t("{host} answers this browser", { host }),
            state: "done",
          }
        : {
            id: "reachable",
            label: t("Waiting for {host} to answer this browser…", { host }),
            state: "active",
            detail:
              reachable === false
                ? t("Not yet. Check that DNS points the name at this server; this keeps trying.")
                : undefined,
          };
  return [
    {
      id: "moved",
      label: address
        ? t("The admin UI is now served at {host}", { host })
        : t("Moving the admin UI…"),
      state: address ? "done" : "active",
    },
    certStep,
    reachStep,
  ];
}

/** Whether the admin may follow the move: every step done. */
export function readyToFollow(steps: MoveStep[]): boolean {
  return steps.every((step) => step.state === "done");
}

/** Whether `origin` answers this browser: its `/health`, fetched without CORS.
 * An opaque answer is an answer — what matters is that DNS resolved, the TLS
 * handshake was accepted and something replied. */
export async function answers(origin: string, fetcher: typeof fetch = fetch): Promise<boolean> {
  try {
    await fetcher(`${origin}/health`, { mode: "no-cors", cache: "no-store" });
    return true;
  } catch {
    return false;
  }
}

/** The base domain applications are served under, once the server has said
 * (`getAdminAddress`, read by the shell). `null` until then, and on a server
 * without one. */
let servedBaseDomain: string | null = null;

/** Record the base domain the server reported. */
export function setServedBaseDomain(base: string | null): void {
  servedBaseDomain = base;
}

/** The host an app is served on: `<subdomain>.<base domain>`, or the base domain
 * itself for the app whose subdomain is `@`. The port is the admin's own.
 *
 * Without a known base domain the admin's own host stands in for it, which is
 * right for as long as the admin UI is on the base domain — `blog.example.com`
 * or, in local dev, `blog.localhost:3032`. Once it has moved to a subdomain of
 * its own (`admin.example.com`) only the base domain the server reports is. */
export function appHost(
  subdomain: string,
  admin: Here = window.location,
  base: string | null = servedBaseDomain,
): string {
  const domain = base ? `${base}${portOf(admin)}` : admin.host;
  return subdomain.trim() === "@" ? domain : `${subdomain}.${domain}`;
}

/** The URL an app is served at (see {@link appHost}). */
export function appUrl(
  subdomain: string,
  admin: Here = window.location,
  base: string | null = servedBaseDomain,
): string {
  return `${admin.protocol}//${appHost(subdomain, admin, base)}`;
}
