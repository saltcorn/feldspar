// The question-mark beside a setting that opens its **help topic** in a popup —
// v1's help icons, next to a field's sublabel (or, without one, its label).
//
// A topic is rendered by the server (`getHelp`, `sc-help`): a markdown template
// filled in with what the screen is about — the table a formula is on, the
// event a code body runs in — so the popup lists *this* table's fields rather
// than describing fields in general. The HTML arrives with raw HTML in the
// template printed as text and links limited to safe schemes, so it is set as
// markup here without a second sanitiser.
//
// Links inside a topic: `help:<topic>` opens that topic in the same popup, and
// anything else opens in a new tab — a link followed from inside a half-filled
// form must not navigate away from it.

import { useEffect, useState, type MouseEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "./api";
import { useT } from "./i18n";
import { IconHelpCircle } from "./icons";

/** What a topic is filled in with: the table and the trigger event the screen
 * is about. Both optional — a topic without them is only less specific. */
export type HelpContext = { table?: string | null; event?: string | null };

/** The topic a link inside a help page names (`help:javascript-code`), or null
 * for an ordinary link. */
export function helpTopicOfLink(href: string | null | undefined): string | null {
  if (!href || !href.toLowerCase().startsWith("help:")) return null;
  const topic = href.slice("help:".length).trim();
  return topic === "" ? null : topic;
}

/** The query `getHelp` is asked with: blank context left out, so the request
 * says only what the screen knows. */
export function helpQuery(context: HelpContext): { table?: string; event?: string } {
  const query: { table?: string; event?: string } = {};
  if (context.table) query.table = context.table;
  if (context.event) query.event = context.event;
  return query;
}

type Page = { title: string; html: string };

/** A small question-mark button that opens help topic `topic` in a popup. */
export function HelpLink({ topic, table, event }: { topic: string } & HelpContext) {
  const { t } = useT();
  // The topic shown, which a `help:` link inside the page can change; reset to
  // the button's own topic every time the popup is opened.
  const [shown, setShown] = useState<string | null>(null);
  const [page, setPage] = useState<Page | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (shown === null) return;
    let live = true;
    setPage(null);
    setError(null);
    api
      .getHelp(shown, helpQuery({ table, event }))
      .then((p) => live && setPage(p))
      .catch((e: unknown) => live && setError(errorMessage(e, t("Could not load the help."))));
    return () => {
      live = false;
    };
    // `t` is left out: a language switch while the popup is open need not
    // refetch the page.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown, table, event]);

  const follow = (e: MouseEvent<HTMLDivElement>) => {
    const link = (e.target as HTMLElement).closest("a");
    if (!link) return;
    const href = link.getAttribute("href");
    e.preventDefault();
    const next = helpTopicOfLink(href);
    if (next) setShown(next);
    else if (href && href !== "#") window.open(href, "_blank", "noopener,noreferrer");
  };

  return (
    <>
      <Button
        variant="link"
        size="sm"
        className="help-link p-0 align-baseline"
        title={t("Help")}
        aria-label={t("Help")}
        onClick={() => setShown(topic)}
      >
        <IconHelpCircle className="icon-1" />
      </Button>
      <Modal show={shown !== null} onHide={() => setShown(null)} size="lg" scrollable>
        <Modal.Header closeButton>
          <Modal.Title>{page?.title ?? t("Help")}</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          {!page && !error && <Spinner animation="border" size="sm" />}
          {page && (
            <div
              className="help-page"
              onClick={follow}
              dangerouslySetInnerHTML={{ __html: page.html }}
            />
          )}
        </Modal.Body>
      </Modal>
    </>
  );
}
