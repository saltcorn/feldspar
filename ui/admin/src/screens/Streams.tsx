// The streams list: every dataflow this installation has been told about, and
// how each one is going *right now* (TODO "Streams", task 7.3).
//
// The models list with one difference that follows from what a stream is: a
// model is at rest, so its list is a read; a stream is not, so this one
// **polls**. What it polls is `streamStatus` per row rather than the whole list
// — the definitions do not move while an admin looks at them, and re-reading a
// provider's configuration ten times a minute to find out whether a broker is
// connected is a lot of database for one badge.
//
// Everything else is the shape every entity list here has: New at the top, and
// Edit, Observe and Delete per row (GOALS names exactly those three). A stream
// that no longer validates — a provider whose module was uninstalled, a topic
// filter that stopped parsing — is **still listed**, marked, with the reason in
// the badge's tooltip, because editing it is the repair.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  counterNotes,
  elementTypeSummary,
  formatCount,
  formatTimestamp,
  readCounters,
  readElementType,
  readStatus,
  statusLabel,
  type Counters,
  type StreamItem,
  type StreamStatusValue,
} from "../streams";
import { T, useT } from "../i18n";

/** How often the list re-reads each row's live status.
 *
 * Two seconds is the Observe socket's own status interval: what is being
 * watched for is a reconnection or a failure, and a list that refreshed ten
 * times a second would cost every open admin tab a request per stream per
 * 100ms for a badge that changes twice a day. */
const POLL_MS = 2000;

/** What the poll knows about one stream, keyed by id. */
type Live = Record<
  string,
  { status: StreamStatusValue | null; counters: Counters; subscribers: number }
>;

export function Streams() {
  const { t } = useT();
  const [streams, setStreams] = useState<StreamItem[] | null>(null);
  const [live, setLive] = useState<Live>({});
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setStreams(await api.listStreams());
      setError(null);
    } catch (err) {
      // Includes the sentence a server built without stream support answers
      // with, which is the honest thing to show on a tab that cannot work.
      setError(errorMessage(err, "Could not load the streams."));
    }
  }, []);

  useEffect(() => {
    void load();
    // The providers are read only for the notice: what this build was made
    // without is a fact about the build, and an empty picker on the stream form
    // reads like a bug where this reads like the decision it is.
    void api
      .listStreamProviders()
      .then((listed) => setNotice(listed.builtins_compiled_out ? (listed.notice ?? null) : null))
      .catch(() => setNotice(null));
  }, [load]);

  // The live half. Definitions are read once; how they are *going* is polled,
  // because a flow is the one entity here whose state moves while nobody is
  // editing it.
  useEffect(() => {
    if (!streams || streams.length === 0) return;
    let cancelled = false;
    const poll = async () => {
      const entries = await Promise.all(
        streams.map(async (stream) => {
          try {
            const status = await api.streamStatus(stream.id);
            return [
              stream.id,
              {
                status: readStatus(status.status),
                counters: readCounters(status.counters),
                subscribers: status.subscribers,
              },
            ] as const;
          } catch {
            // A stream that has just been deleted in another tab, or a server
            // that went away: the row keeps whatever it last knew rather than
            // flickering to "not running".
            return null;
          }
        }),
      );
      if (cancelled) return;
      setLive((current) => {
        const next = { ...current };
        for (const entry of entries) if (entry) next[entry[0]] = entry[1];
        return next;
      });
    };
    void poll();
    const timer = window.setInterval(() => void poll(), POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [streams]);

  const remove = async (stream: StreamItem) => {
    if (
      !window.confirm(
        t(
          'Remove the stream "{name}"?\n\nNothing that has already arrived is lost — an element is not stored — but anything observing this stream stops, and a trigger that names it will have to be pointed somewhere else.',
          { name: stream.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteStream(stream.id);
      await load();
    } catch (err) {
      // Includes the refusal a stream with triggers on it answers with, which
      // names them: that message is the whole point of the refusal.
      setError(errorMessage(err, "Could not remove the stream."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Dataflows"
        title={t("Streams")}
        actions={
          <Button onClick={() => navigate("/streams/new")}>
            <IconPlus className="icon-2" />
            <T text="New stream" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {notice && <Alert variant="info">{notice}</Alert>}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Provider" /></th>
                <th><T text="Elements" /></th>
                <th title={t("Live subscriptions from applications' pages on this server, right now")}>
                  <T text="Subscribers" />
                </th>
                <th><T text="Status" /></th>
                <th><T text="Last element" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {streams?.length === 0 && (
                <tr>
                  <td colSpan={7} className="text-muted">
                    <T text="No streams yet. A stream is a provider — an MQTT broker, a polled feed — with its settings filled in, and what arrives on it is not stored: a trigger that writes a row is what makes a flow durable." />
                  </td>
                </tr>
              )}
              {streams?.map((stream) => {
                const seen = live[stream.id];
                const counters = seen?.counters ?? readCounters(stream.counters);
                const status = seen ? seen.status : readStatus(stream.status);
                const badge = statusLabel(status, stream.enabled);
                const notes = counterNotes(counters);
                const subscribers = seen?.subscribers ?? stream.subscribers;
                return (
                  <tr key={stream.id}>
                    <td>
                      {stream.name}
                      {stream.description && (
                        <div className="text-muted small">{stream.description}</div>
                      )}
                      {/* The reason lives in the tooltip: it is a sentence, and
                          a sentence in a table cell would push the row apart. */}
                      {stream.error && (
                        <StatusBadge tone="red" title={stream.error} className="mt-1">
                          <T text="Cannot be started" />
                        </StatusBadge>
                      )}
                    </td>
                    <td>
                      {stream.provider}
                      <div className="text-muted small">
                        {elementTypeSummary(readElementType(stream.element_type))}
                      </div>
                    </td>
                    <td>
                      {formatCount(counters.elements)}
                      {/* §7: a stream that is dropping is a thing you can see.
                          Silent while the counters are zero, so the line means
                          something when it is there. */}
                      {notes.length > 0 && (
                        <div className="text-warning small">{notes.join(" · ")}</div>
                      )}
                    </td>
                    <td className={subscribers === 0 ? "text-muted" : undefined}>
                      {formatCount(subscribers)}
                    </td>
                    <td>
                      <StatusBadge tone={badge.tone} title={badge.title}>
                        {badge.label}
                      </StatusBadge>
                    </td>
                    <td className="text-muted">{formatTimestamp(counters.last_element_at)}</td>
                    <td className="text-end">
                      <div className="btn-list justify-content-end flex-nowrap align-items-center">
                        <Button
                          size="sm"
                          variant="primary"
                          href={`#/streams/${encodeURIComponent(stream.id)}/observe`}
                        >
                          <T text="Observe" />
                        </Button>
                        <Button
                          size="sm"
                          variant="outline-secondary"
                          href={`#/streams/${encodeURIComponent(stream.id)}/edit`}
                        >
                          <T text="Edit" />
                        </Button>
                        <Button
                          size="sm"
                          variant="outline-danger"
                          onClick={() => void remove(stream)}
                        >
                          <T text="Delete" />
                        </Button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        </div>
      </PageBody>
    </>
  );
}
