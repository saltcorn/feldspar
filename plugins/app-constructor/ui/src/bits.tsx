// The small admin pieces the screen uses, copied rather than imported: the
// admin's modules are not an API. From ui/admin/src: ClampedText.tsx,
// inlineCode.tsx, history.ts (`historyTime`), runStory.ts (`firstSentence`),
// screens/TableDetail.tsx (`tableHref`) and layout.tsx (`PageHeader`,
// `StatusBadge`).

import {
  createContext,
  useContext,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";

import { t } from "./i18n";

// --- links within the screen --------------------------------------------------

/** How the screen links to its own paths: `host.href`. */
export type Links = { href: (path: string) => string };

const LinksContext = createContext<Links>({ href: (path) => `#${path}` });

export const LinksProvider = LinksContext.Provider;

/** The link to one of the screen's own paths (`/phases/3`). */
export function useHref(): (path: string) => string {
  return useContext(LinksContext).href;
}

// --- links to the admin's own pages -----------------------------------------
// The screen renders inside the admin's page, so its hash routes work.

/** A table's page in the admin. */
export function tableHref(table: string): string {
  return `#/tables/${encodeURIComponent(table)}`;
}

// --- text ---------------------------------------------------------------------

/** A time the server sent, in the admin's locale. Left as sent if it does not
 * parse. */
export function historyTime(at: string): string {
  const time = new Date(at);
  return Number.isNaN(time.getTime()) ? at : time.toLocaleString();
}

/** The first sentence of what an agent wrote, for a one-line gist: its first
 * line that is not blank, without a list marker or bold, up to the first
 * sentence end outside a code span (one after only a word or two, as in
 * "e.g.", is passed over), at most `max` characters. */
export function firstSentence(text: string, max = 160): string {
  const line =
    text
      .split("\n")
      .map((l) =>
        l
          .replace(/^\s*(#+|[-*]|\d+\.)\s+/, "")
          .replace(/\*\*/g, "")
          .trim()
      )
      .find((l) => l !== "") ?? "";
  let end = line.length;
  let inCode = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === "`") inCode = !inCode;
    else if (
      !inCode &&
      ".!?:".includes(c) &&
      (i + 1 === line.length || line[i + 1] === " ")
    ) {
      const words = line.slice(0, i).trim().split(/\s+/).length;
      if (words <= 2 && i + 1 < line.length) continue;
      end = i + 1;
      break;
    }
  }
  const sentence = line.slice(0, end);
  return sentence.length > max
    ? `${sentence.slice(0, max - 1).trimEnd()}…`
    : sentence;
}

/** A run of text, or a `code` span in it. An unpaired backtick, or one whose
 * pair is on another line, is left as text. */
export function codeSpans(text: string): { code: boolean; text: string }[] {
  const parts: { code: boolean; text: string }[] = [];
  const span = /`([^`\n]+)`/g;
  let at = 0;
  let match: RegExpExecArray | null;
  while ((match = span.exec(text)) !== null) {
    if (match.index > at)
      parts.push({ code: false, text: text.slice(at, match.index) });
    parts.push({ code: true, text: match[1] });
    at = match.index + match[0].length;
  }
  if (at < text.length) parts.push({ code: false, text: text.slice(at) });
  return parts;
}

/** Text with its backticked names set as `<code>`. */
export function InlineCode({ text }: { text: string }) {
  return (
    <>
      {codeSpans(text).map((part, k) =>
        part.code ? (
          <code key={k}>{part.text}</code>
        ) : (
          <span key={k}>{part.text}</span>
        )
      )}
    </>
  );
}

/** Long text an agent wrote — a task's summary, an error — shown a few lines
 * deep until opened, with its backticked names set as code. */
export function ClampedText({
  text,
  lines = 3,
  className = "build-task-summary",
}: {
  text: string;
  /** How many lines show while it is closed. */
  lines?: number;
  /** The text's own look; the clamp is added to it. */
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  // Short text needs no toggle: about as many characters as fit in the lines
  // shown, and no more lines than that.
  const long = text.length > lines * 95 || text.split("\n").length > lines;
  const closed = long && !open;
  return (
    <>
      <div
        className={`${className}${closed ? " clamp-closed" : ""}`}
        style={{ "--clamp": lines } as CSSProperties}
      >
        <InlineCode text={text} />
      </div>
      {long && (
        <button
          type="button"
          className="build-more"
          onClick={() => setOpen(!open)}
        >
          {open ? t("Show less") : t("Show more")}
        </button>
      )}
    </>
  );
}

// --- layout -------------------------------------------------------------------

/** The screen's header: its pretitle, title and actions, as the admin's page
 * header lays them out. The admin already puts the screen in its page body's
 * container, so this one has none of its own. */
export function PageHeader({
  title,
  pretitle,
  actions,
}: {
  title: ReactNode;
  pretitle?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div
      className="page-header d-print-none mt-0 mb-3"
      aria-label={t("Page header")}
    >
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
  );
}

/** The colours a status badge comes in, named as Tabler names them. */
export type Tone = "green" | "red" | "yellow" | "blue" | "secondary";

/** A status badge in Tabler's soft colours (`bg-<tone>-lt`), which stay
 * readable where Bootstrap's `<Badge bg=…>` comes out grey on grey. */
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
    <span
      className={`badge bg-${tone}-lt${className ? ` ${className}` : ""}`}
      title={title}
    >
      {children}
    </span>
  );
}
