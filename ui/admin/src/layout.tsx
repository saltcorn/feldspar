// The Tabler page furniture every admin screen sits in.
//
// Tabler's vertical layout is a fixed shape: a `.page` holding the sidebar and a
// `.page-wrapper`, and inside the wrapper a `.page-header` (title, pre-title,
// actions) followed by a `.page-body` (a `.container-xl` of cards). The header
// is a *sibling* of the body, not a heading inside it — it has its own spacing
// and its own print rules — so a screen renders the two parts itself rather than
// the shell wrapping everything in one container. These two components are that
// contract, in one place, so a screen states its title and its actions and gets
// the same header as every other screen.
//
// `CenteredPage` is the other Tabler page shape: the signed-out one, a narrow
// column centred in the viewport, used by the login and first-user screens.

import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useT } from "./i18n";

/** The page header: pre-title, title, and the screen's primary actions.
 *
 * `actions` land in Tabler's `.btn-list`, which handles the spacing between
 * buttons — so a screen passes bare buttons, not a hand-spaced `<div>`. */
export function PageHeader({
  title,
  pretitle,
  actions,
}: {
  title: ReactNode;
  pretitle?: ReactNode;
  actions?: ReactNode;
}) {
  const { t } = useT();
  return (
    <div className="page-header d-print-none" aria-label={t("Page header")}>
      <div className="container-xl">
        <div className="row g-2 align-items-center">
          <div className="col">
            {pretitle && <div className="page-pretitle">{pretitle}</div>}
            <h2 className="page-title">{title}</h2>
          </div>
          {actions && (
            <div className="col-auto ms-auto d-print-none">
              <div className="btn-list">{actions}</div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/** The page body: the container a screen's cards go in. */
export function PageBody({ children }: { children: ReactNode }) {
  return (
    <div className="page-body">
      <div className="container-xl">{children}</div>
    </div>
  );
}

/** The colours a status badge comes in, named as Tabler names them. */
export type Tone = "green" | "red" | "yellow" | "blue" | "secondary";

/** A status badge: "Enabled", "Not connected", "Build failed".
 *
 * Tabler's `.badge` colours its *text* from `--tblr-badge-color` and takes its
 * background from a `bg-…` class, so Bootstrap's `<Badge bg="secondary">` comes
 * out grey-on-grey and unreadable. The two combinations Tabler actually ships
 * are `bg-<tone>` + `text-<tone>-fg` (solid) and `bg-<tone>-lt` (soft); this
 * uses the soft one, which is what its own tables use, and having it in one
 * place is what stops the unreadable combination coming back.
 *
 * `title` carries the detail — a formula, an error — for the marks that have
 * one, which is why a badge is enough and a sentence is not needed. */
export function StatusBadge({
  tone,
  title,
  className,
  children,
}: {
  tone: Tone;
  title?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <span className={`badge bg-${tone}-lt${className ? ` ${className}` : ""}`} title={title}>
      {children}
    </span>
  );
}

/** "Public" beside an endpoint whose role floor is 100 (TODO F1.6): anyone
 * may call it, signed in or not. Shown wherever min_role 100 is, because a
 * number is easy to read past. */
export function PublicBadge({ className }: { className?: string }) {
  return (
    <StatusBadge
      tone="yellow"
      className={className}
      title="Minimum role 100: anyone may call this, signed in or not"
    >
      Public
    </StatusBadge>
  );
}

/** An alert's body.
 *
 * Tabler's `.alert` is a flex **row** (its first column is the optional icon),
 * so an alert with several children lays them out side by side instead of
 * stacking them. Wrapping the content in one element gives the row one column,
 * which is the layout every alert here wants. */
export function AlertBody({ children }: { children: ReactNode }) {
  return <div className="flex-fill">{children}</div>;
}

/** A message over whatever screen the admin is on: Tabler's toast, top right,
 * until dismissed.
 *
 * It does **not** time out, and that is the point of using one here: what a
 * toast carries on these screens is the outcome of something the admin asked
 * for — a build's diagnostics, a test run's error and what the body printed —
 * and a message that disappears while it is being read is a message that has to
 * be produced again. The dismissal is the admin saying they have finished with
 * it.
 *
 * `children` is the body, so a caller may show more than a line of text (a
 * transcript, a result) while the heading and the colour stay the same
 * everywhere. */
export function NoticeToast({
  ok,
  title,
  onClose,
  children,
}: {
  ok: boolean;
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  const { t } = useT();
  return (
    <div className="toast-container position-fixed top-0 end-0 p-3">
      <div
        className={`toast show app-outcome-toast border-${ok ? "success" : "danger"}`}
        role={ok ? "status" : "alert"}
        aria-live={ok ? "polite" : "assertive"}
      >
        <div className="toast-header">
          <strong className={`me-auto text-${ok ? "success" : "danger"}`}>{title}</strong>
          <button
            type="button"
            className="btn-close"
            aria-label={t("Close")}
            onClick={onClose}
          />
        </div>
        <div className="toast-body">{children}</div>
      </div>
    </div>
  );
}

/** The signed-out page shape: a narrow, vertically centred column. */
export function CenteredPage({ children }: { children: ReactNode }) {
  return (
    <div className="page page-center">
      <div className="container container-tight py-4">{children}</div>
    </div>
  );
}

/** The two colour schemes Tabler ships. */
export type Theme = "light" | "dark";

const THEME_KEY = "saltcorn-admin-theme";

/** Read the stored preference, falling back to what the OS reports. */
function initialTheme(): Theme {
  const stored = window.localStorage.getItem(THEME_KEY);
  if (stored === "light" || stored === "dark") return stored;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

const FOLDED_KEY = "saltcorn-admin-sidebar-folded";

/** Whether the sidebar is folded to a rail of icons, remembered across visits.
 *
 * A preference rather than a route or a viewport question: an admin working in
 * the row editor wants the width back, and wants it to still be theirs on the
 * next screen and the next visit. Unfolded is the default. Only meaningful on
 * wide screens — below Tabler's `lg` breakpoint the sidebar is a drawer rather
 * than a rail, and every rule keyed on the folded state is inside that
 * breakpoint. This hook is not, so the stored preference outlives a window
 * resize. */
export function useFoldedSidebar(): [boolean, () => void] {
  const [folded, setFolded] = useState(
    () => window.localStorage.getItem(FOLDED_KEY) === "true",
  );

  useEffect(() => {
    window.localStorage.setItem(FOLDED_KEY, String(folded));
  }, [folded]);

  return [folded, useCallback(() => setFolded((f) => !f), [])];
}

/** The colour scheme, applied to the document and remembered across visits.
 *
 * Tabler switches on `data-bs-theme` on the root element — the same attribute
 * Bootstrap 5.3 uses — so setting it is the whole implementation, and doing it
 * from React means the theme script Tabler's static templates load is not
 * needed (it would be a second inline/vendored script for one attribute). */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(initialTheme);

  useEffect(() => {
    document.documentElement.setAttribute("data-bs-theme", theme);
    window.localStorage.setItem(THEME_KEY, theme);
  }, [theme]);

  const toggle = useCallback(() => {
    setTheme((current) => (current === "dark" ? "light" : "dark"));
  }, []);

  return [theme, toggle];
}
