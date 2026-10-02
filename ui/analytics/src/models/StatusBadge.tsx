// A status badge — "fitted", "active", "Cannot be fitted" — as the admin UI
// draws one (`ui/admin/src/layout.tsx`): Tabler's soft `bg-<tone>-lt`, the
// one combination of its badge colours that reads in both themes.

import type { ReactNode } from "react";

/** The colours a status badge comes in, named as Tabler names them. */
export type Tone = "green" | "red" | "yellow" | "blue" | "secondary";

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

/** How a fit's status is coloured: one still running is blue rather than
 * green, because it has not answered anything yet. */
export function fitTone(status: string): Tone {
  if (status === "fitted") return "green";
  if (status === "failed") return "red";
  return "blue";
}
