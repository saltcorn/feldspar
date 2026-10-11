// Observe: watching a stream's elements arrive, live (TODO "Streams", task
// 7.3, §9).
//
// The one admin screen in this tree whose content is not a read. There is no
// "the elements" to `GET` — an element is not stored (§4) — so this is a socket
// that is handed each element as it arrives, and everything on the screen
// follows from that:
//
//   - **Pause and Clear are client-side only.** Pausing the server would mean
//     either dropping the elements or queueing them, and §7 says which of those
//     a stream does — so Pause stops the *rendering* and says how many arrived
//     while it was held, rather than pretending the flow waited.
//   - **The history says what it is.** The socket replays the last hundred
//     envelopes *this process* saw, and those rows are labelled "since this
//     server started", which is the whole truth: there is no element table and
//     there is not going to be one.
//   - **A lag is shown, never hidden.** A consumer that cannot keep up is the
//     consumer's problem (§7), and a gap this screen names beats a gap it
//     silently draws.
//
// The rendering follows the declared element type, which is what declaring one
// is for: a table of the declared keys for `json`, a text tail for `text`, and
// a hex head for `binary` — never bytes set in a body font.
//
// A stream with topics (TODO.md "Live updates" L2.9) — a user's id, a row's key
// — gets a **topic** column and a box that narrows the tail to one topic. The
// admin's socket watches every topic, which an application's page never does.
//
// A stream with presence (L4.5) gets a **Present now** card: per topic, who has
// it open — the user, the fields the stream shows the members, and the state
// the page last set — exactly what the members see of each other.

import { useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { IconArrowLeft, IconPlayerStop, IconTrash } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  applyFrame,
  cellText,
  counterNotes,
  elementCells,
  elementColumns,
  elementText,
  elementTypeSummary,
  emptyObserve,
  extraKeys,
  filterByTopic,
  formatCount,
  formatTime,
  hexHead,
  parseFrame,
  readTopics,
  statusLabel,
  streamObserveUrl,
  topicLabel,
  type ElementType,
  type Envelope,
  type ObserveState,
  type PresenceTopic,
  type TopicKind,
} from "../streams";
import { T, useT } from "../i18n";

export function StreamObserve({ streamId }: { streamId: string }) {
  const { t } = useT();
  const [state, setState] = useState<ObserveState>(emptyObserve());
  const [name, setName] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [topics, setTopics] = useState<{ kind: TopicKind; table?: string }>({ kind: "single" });
  /** What the topic box narrows the tail to; blank is every topic. */
  const [topicQuery, setTopicQuery] = useState("");
  const [paused, setPaused] = useState(false);
  /** Elements that arrived while paused — counted, because the flow did not
   * wait and saying nothing would be claiming it did. */
  const [held, setHeld] = useState(0);
  /** The newest envelope at the moment Clear was pressed: everything from it
   * down is hidden. A *mark* rather than a count, because the tail is bounded
   * and a count would go stale the moment the far end started falling off —
   * and a mark that falls off simply stops matching, which shows everything
   * again, which is the right answer for a tail that has turned over
   * completely since it was cleared. */
  const [clearMark, setClearMark] = useState<Envelope | null>(null);

  // The row, for the heading and the "disabled" case. One read: a definition
  // does not move while a socket is open, and what does move arrives on it.
  useEffect(() => {
    let cancelled = false;
    void api
      .getStream(streamId)
      .then((stream) => {
        if (cancelled) return;
        setName(stream.name);
        setEnabled(stream.enabled);
        setTopics(readTopics(stream.topics));
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setState((s) => ({ ...s, error: errorMessage(err, "That stream no longer exists.") }));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [streamId]);

  // `paused` is read inside the socket's message handler, which is installed
  // once; a ref is what lets the handler see the current value without the
  // socket being torn down and reconnected on every pause.
  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  useEffect(() => {
    const socket = new WebSocket(streamObserveUrl(window.location, streamId));
    socket.onmessage = (event: MessageEvent) => {
      const frame = parseFrame(typeof event.data === "string" ? event.data : "");
      if (!frame) return;
      if (frame.type === "element" && pausedRef.current) {
        setHeld((n) => n + 1);
        return;
      }
      setState((current) => applyFrame(current, frame));
    };
    socket.onerror = () => {
      setState((s) => ({ ...s, error: "The connection to the server failed." }));
    };
    socket.onclose = (event: CloseEvent) => {
      // The server closes with a *reason* rather than a status once the
      // handshake has succeeded — "this stream is not running on this server"
      // — and that sentence is the answer to what the screen is showing
      // nothing for.
      setState((s) => ({
        ...s,
        error: event.reason || (s.ready ? "The connection closed." : null),
      }));
    };
    return () => socket.close();
  }, [streamId]);

  const badge = statusLabel(state.status, enabled);
  const notes = counterNotes(state.counters);
  const cleared = useMemo(() => {
    if (!clearMark) return state.tail;
    const cut = state.tail.indexOf(clearMark);
    return cut === -1 ? state.tail : state.tail.slice(0, cut);
  }, [state.tail, clearMark]);
  const tail = useMemo(() => filterByTopic(cleared, topicQuery), [cleared, topicQuery]);
  /** Whether a row is history: the replay is the oldest `replayed` envelopes of
   * the whole tail, which the filter keeps in order, so a row is history when
   * it sits at or past the replay's start there. */
  const historyFrom = state.tail.length - state.replayed;
  const isHistory = (envelope: Envelope) => state.tail.indexOf(envelope) >= historyFrom;
  /** A column for the topic whenever the stream has topics, or an element
   * carries one anyway. */
  const showTopic = topics.kind !== "single" || state.tail.some((e) => e.topic !== undefined);
  const heading = topicLabel(t, topics);

  return (
    <>
      <PageHeader
        pretitle="Stream"
        title={name || "Observe"}
        actions={
          <div className="btn-list">
            <Button
              variant={paused ? "primary" : "outline-secondary"}
              onClick={() => {
                setPaused((p) => !p);
                setHeld(0);
              }}
            >
              <IconPlayerStop className="icon-2" />
              {paused ? "Resume" : "Pause"}
            </Button>
            <Button
              variant="outline-secondary"
              onClick={() => {
                setClearMark(state.tail[0] ?? null);
                setHeld(0);
              }}
            >
              <IconTrash className="icon-2" />
              <T text="Clear" />
            </Button>
            <Button variant="outline-secondary" href="#/streams">
              <IconArrowLeft className="icon-2" />
              <T text="Streams" />
            </Button>
          </div>
        }
      />
      <PageBody>
        {state.error && <Alert variant="danger">{state.error}</Alert>}

        <div className="card mb-3">
          <div className="card-body d-flex flex-wrap gap-3 align-items-center">
            <StatusBadge tone={badge.tone} title={badge.title}>
              {badge.label}
            </StatusBadge>
            <span className="text-muted">
              {elementTypeSummary(state.elementType)}
            </span>
            <span className="text-muted">
              {t("{count} elements since this server started", {
                count: formatCount(state.counters.elements),
              })}
            </span>
            {notes.length > 0 && <span className="text-warning">{notes.join(" · ")}</span>}
          </div>
        </div>

        {/* §7: told, never hidden. */}
        {state.lagged > 0 && (
          <Alert variant="warning">
            {t(
              "This screen fell behind and lost {count} elements. Nothing back-pressures a stream: a consumer that cannot keep up is skipped rather than made to wait, and the elements that were skipped are gone.",
              { count: formatCount(state.lagged) },
            )}
          </Alert>
        )}
        {paused && (
          <Alert variant="info">
            {t(
              "Paused — {count} elements have arrived and are not shown. Pausing stops this screen, not the stream.",
              { count: formatCount(held) },
            )}
          </Alert>
        )}

        {state.presence !== null && (
          <PresenceCard topics={state.presence} heading={heading} />
        )}

        {showTopic && (
          <Form.Group className="mb-3" style={{ maxWidth: "24rem" }} controlId="observe-topic">
            <Form.Label>{heading}</Form.Label>
            <Form.Control
              type="search"
              placeholder={t("Show one topic — part of a user id or row key")}
              value={topicQuery}
              onChange={(event) => setTopicQuery(event.target.value)}
            />
          </Form.Group>
        )}

        {state.ready && tail.length === 0 && !state.error && (
          <Alert variant="info">
            <T text="Nothing yet. Elements appear here as they arrive; there is no history beyond what this server has seen since it started, because an element is not stored." />
          </Alert>
        )}

        {tail.length > 0 && (
          <div className="card">
            <ElementTail
              tail={tail}
              type={state.elementType}
              isHistory={isHistory}
              topicColumn={showTopic ? heading : null}
            />
          </div>
        )}
      </PageBody>
    </>
  );
}

/** Who has each topic open now. */
function PresenceCard({ topics, heading }: { topics: PresenceTopic[]; heading: string }) {
  const { t } = useT();
  return (
    <div className="card mb-3">
      <div className="card-header">
        <h3 className="card-title"><T text="Present now" /></h3>
      </div>
      {topics.length === 0 ? (
        <div className="card-body text-muted">
          <T text="Nobody has this stream open." />
        </div>
      ) : (
        <Table responsive className="card-table table-vcenter">
          <thead>
            <tr>
              <th>{heading}</th>
              <th><T text="User" /></th>
              <th><T text="Shown fields" /></th>
              <th><T text="State" /></th>
            </tr>
          </thead>
          <tbody>
            {topics.flatMap(({ topic, members }) =>
              members.map((member, index) => (
                <tr key={`${topic ?? ""}-${member.connection}`}>
                  <td className="small font-monospace text-break">
                    {index === 0 ? (topic ?? "—") : ""}
                  </td>
                  <td className="small font-monospace text-break">
                    {member.user_id ?? t("anonymous")}
                  </td>
                  <td className="small">
                    {Object.keys(member.fields).length === 0
                      ? "—"
                      : Object.entries(member.fields)
                          .map(([name, value]) => `${name}: ${cellText(value)}`)
                          .join(", ")}
                  </td>
                  <td className="small font-monospace text-break">
                    {member.state === null ? "—" : cellText(member.state)}
                  </td>
                </tr>
              )),
            )}
          </tbody>
        </Table>
      )}
    </div>
  );
}

/** The tail, rendered against the declared element type. */
function ElementTail({
  tail,
  type,
  isHistory,
  topicColumn,
}: {
  tail: Envelope[];
  type: ElementType | null;
  isHistory: (envelope: Envelope) => boolean;
  /** The topic column's heading, or `null` for a stream without topics. */
  topicColumn: string | null;
}) {
  const columns = elementColumns(type);
  return (
    <Table hover responsive className="card-table table-vcenter">
      <thead>
        <tr>
          <th><T text="Received" /></th>
          {topicColumn !== null && <th>{topicColumn}</th>}
          {type?.kind === "json" ? (
            columns.map((column) => <th key={column}>{column}</th>)
          ) : (
            <th><T text="Value" /></th>
          )}
          <th><T text="Source" /></th>
        </tr>
      </thead>
      <tbody>
        {tail.map((envelope, index) => (
          <tr key={`${envelope.received_at}-${index}`} className={isHistory(envelope) ? "text-muted" : undefined}>
            <td className="text-nowrap">
              {formatTime(envelope.received_at)}
              {/* The whole truth about what this row is (§9). */}
              {isHistory(envelope) && (
                <div className="small"><T text="since this server started" /></div>
              )}
            </td>
            {topicColumn !== null && (
              <td className="small font-monospace text-break">{envelope.topic ?? "—"}</td>
            )}
            <ValueCells envelope={envelope} type={type} columns={columns} />
            <td className="small font-monospace text-muted">
              {envelope.source === undefined ? "—" : cellText(envelope.source)}
            </td>
          </tr>
        ))}
      </tbody>
    </Table>
  );
}

/** One element's value, in as many cells as its type asks for. */
function ValueCells({
  envelope,
  type,
  columns,
}: {
  envelope: Envelope;
  type: ElementType | null;
  columns: string[];
}) {
  if (type?.kind === "json") {
    const extra = extraKeys(envelope, type);
    return (
      <>
        {elementCells(envelope, type).map((cell, index) => (
          <td key={columns[index]} className="font-monospace small">
            {cell}
            {/* Undeclared keys are carried through rather than dropped (§4);
                naming them on the first column is what tells an admin there is
                something to add to the declaration. */}
            {index === 0 && extra.length > 0 && (
              <div className="text-muted">+ {extra.join(", ")}</div>
            )}
          </td>
        ))}
      </>
    );
  }
  if (type?.kind === "binary") {
    const value = typeof envelope.value === "string" ? envelope.value : "";
    const head = hexHead(value);
    return (
      <td className="font-monospace small">
        {head.length === null ? "—" : `${head.hex}  (${formatCount(head.length)} bytes)`}
      </td>
    );
  }
  return <td className="font-monospace small text-break">{elementText(envelope)}</td>;
}
