// A formula, typed, with the names it may use offered as it is typed
// (analytics TODO A1.17): the stage's columns, the fields one step along each
// foreign key (`neighbourhoodⱵname`), and — while rows are a table's rows —
// the counts and totals of its child tables (`viewingsↃhouse.length`).
//
// Up and down move through the offers, Enter or Tab takes one, Escape closes
// them. Taking one replaces the identifier the cursor is in, so typing
// `neighbourhoodⱵn` and taking `neighbourhoodⱵname` gives `neighbourhoodⱵname`.

import { useMemo, useRef, useState } from "react";
import Form from "react-bootstrap/Form";

import { useT } from "../i18n";
import { applyCompletion, matchCompletions, tokenAt, type Completion } from "./ops";

export function FormulaInput({
  value,
  onChange,
  completions,
  id,
  placeholder,
  autoFocus,
}: {
  value: string;
  onChange: (value: string) => void;
  completions: Completion[];
  id?: string;
  placeholder?: string;
  autoFocus?: boolean;
}) {
  const { t } = useT();
  const input = useRef<HTMLInputElement | null>(null);
  const [cursor, setCursor] = useState(0);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);

  const offered = useMemo(
    () => (open ? matchCompletions(completions, tokenAt(value, cursor).prefix) : []),
    [open, completions, value, cursor],
  );

  const take = (c: Completion) => {
    const next = applyCompletion(value, cursor, c.text);
    onChange(next.text);
    setOpen(false);
    window.requestAnimationFrame(() => {
      input.current?.setSelectionRange(next.cursor, next.cursor);
      input.current?.focus();
    });
  };

  const kindLabel = (c: Completion) =>
    c.kind === "join" ? t("join") : c.kind === "aggregation" ? t("aggregation") : c.detail ?? "";

  return (
    <div className="an-formula">
      <Form.Control
        ref={input}
        id={id}
        className="font-monospace"
        value={value}
        placeholder={placeholder}
        autoFocus={autoFocus}
        autoComplete="off"
        spellCheck={false}
        onChange={(e) => {
          onChange(e.target.value);
          setCursor(e.target.selectionStart ?? e.target.value.length);
          setOpen(true);
          setActive(0);
        }}
        onKeyUp={(e) => setCursor(e.currentTarget.selectionStart ?? 0)}
        onClick={(e) => setCursor(e.currentTarget.selectionStart ?? 0)}
        onBlur={() => window.setTimeout(() => setOpen(false), 150)}
        onKeyDown={(e) => {
          if (offered.length === 0) return;
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setActive((a) => (a + 1) % offered.length);
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setActive((a) => (a - 1 + offered.length) % offered.length);
          } else if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            take(offered[Math.min(active, offered.length - 1)]);
          } else if (e.key === "Escape") {
            setOpen(false);
          }
        }}
      />
      {offered.length > 0 && (
        <div className="an-completions" role="listbox">
          {offered.map((c, i) => (
            <div
              key={c.text}
              role="option"
              aria-selected={i === active}
              className={`an-completion${i === active ? " active" : ""}`}
              onMouseDown={(e) => {
                e.preventDefault();
                take(c);
              }}
            >
              <span>{c.text}</span>
              <span className="text-secondary">{kindLabel(c)}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
