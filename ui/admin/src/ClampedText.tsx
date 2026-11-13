// Long text an agent wrote — a task's brief or summary, a run's task or answer
// — shown a few lines deep until opened, with its backticked names set as code.

import { useState, type CSSProperties } from "react";

import { useT } from "./i18n";
import { InlineCode } from "./inlineCode";

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
  const { t } = useT();
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
        <button type="button" className="build-more" onClick={() => setOpen(!open)}>
          {open ? t("Show less") : t("Show more")}
        </button>
      )}
    </>
  );
}
