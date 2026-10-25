// Triggers list: every stored trigger, what fires it, what it runs, and whether
// it can run at all (§10.2).
//
// Two things this screen exists to make visible, both of them states a list of
// names could not show:
//
//   - A trigger can be **stored but not usable** — its table dropped, its action
//     provided by a plugin that is gone, its formula no longer resolving. It is
//     not in the live set and will not fire, but it is still here, with the
//     reason, and still editable, because editing it is the repair.
//   - A trigger with **no intrinsic event** (`none`) only ever runs because
//     something asks it to, so this is where the asking happens. But every
//     trigger can be **test run** from here, not only that one: a trigger on a
//     table is run against a row picked at random from it, so "does this
//     actually work?" is a question about any row of them rather than a
//     question an admin has to answer by writing a row and watching. What comes
//     back is the result, or the failure, and what a code body **printed** on
//     its way to either — which is the answer to the next question, and the one
//     a hosted deployment has no terminal to read off.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListTriggersResponse } from "../client";
import { navigate } from "../App";
import { IconPlus } from "../icons";
import { NoticeToast, PageBody, PageHeader, StatusBadge } from "../layout";
import { T, useT } from "../i18n";
import {
  testRunFailure,
  testRunOutcome,
  type TestRunOutcome,
} from "../triggerTestRun";

type TriggerItem = ListTriggersResponse[number];

/** What one trigger runs, in one line: the action and the table it targets, if
 * its configuration names one. Deliberately generic — the screen knows no
 * action's settings, so it shows the `table` setting when there is one rather
 * than reaching into a particular action's shape. */
/** Days as the server numbers them: 0 = Monday … 6 = Sunday. */
const DAYS = [
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
  "Sunday",
];

/** A periodic trigger's timing in words, or `null` for a kind that has none.
 *
 * Read from the same three fields the form writes, so what the list says and
 * what the scheduler does come from one description. Everything is UTC (§10.2's
 * decision 6), and the label says so — an admin who reads "03:00" and assumes
 * local time is an admin whose nightly job runs at the wrong hour. */
function scheduleSummary(trigger: TriggerItem): string | null {
  const minute = trigger.minute ?? 0;
  const hour = trigger.hour ?? 0;
  const pad = (n: number) => String(n).padStart(2, "0");
  switch (trigger.when) {
    case "often":
      return "every 5 minutes";
    case "hourly":
      return `at :${pad(minute)} past the hour`;
    case "daily":
      return `at ${pad(hour)}:${pad(minute)} UTC`;
    case "weekly":
      return `${DAYS[trigger.day_of_week ?? 0]} at ${pad(hour)}:${pad(minute)} UTC`;
    default:
      return null;
  }
}

/** The last run as a local, readable time — or a dash for one that has never
 * run, which for a scheduled trigger is itself worth seeing. */
function lastRun(trigger: TriggerItem): string {
  if (!trigger.last_run_at) return "—";
  const at = new Date(trigger.last_run_at);
  return Number.isNaN(at.getTime()) ? trigger.last_run_at : at.toLocaleString();
}

export function targetSummary(trigger: TriggerItem): string {
  // A workflow body runs a program rather than an action, and has no
  // configuration to read a table out of (§10.3). "workflow" alone tells one
  // trigger from another not at all, so the list says how big the program is and
  // which version is live — the two facts that make a row identifiable.
  const action = trigger.action ?? "";
  if (trigger.body === "workflow") {
    if (trigger.workflow_version == null) return "workflow";
    const steps = trigger.workflow_steps ?? 0;
    return `workflow · ${steps} step${steps === 1 ? "" : "s"} · v${trigger.workflow_version}`;
  }
  const config = trigger.configuration;
  if (!config || typeof config !== "object") return action;
  const table = (config as Record<string, unknown>).table;
  return typeof table === "string" && table !== ""
    ? `${action} → ${table}`
    : action;
}

export function Triggers() {
  const { t } = useT();
  const [triggers, setTriggers] = useState<TriggerItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** The last test run, shown in a toast until it is dismissed or another one
   * replaces it. A toast rather than a banner because it is news about
   * something that has finished, not a state of this screen — and it does not
   * time out, because a transcript that vanishes while it is being read has to
   * be produced again. */
  const [ran, setRan] = useState<TestRunOutcome | null>(null);
  /** The trigger a test run is in flight for, so its own button says so. */
  const [running, setRunning] = useState<string | null>(null);

  const load = async () => {
    try {
      setTriggers(await api.listTriggers());
    } catch {
      setError("Could not load triggers.");
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const remove = async (trigger: TriggerItem) => {
    if (
      !window.confirm(
        t(
          'Delete the trigger "{name}"?\n\nIts configuration is deleted with it. Switch it off instead if you only want it to stop firing.',
          { name: trigger.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteTrigger(trigger.id);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not delete the trigger."));
    }
  };

  /** Run one trigger now and show what happened.
   *
   * The action's own failure is **not** an error of this screen: it comes back
   * as a 200 saying so, and it goes in the same toast the success goes in,
   * because it is the answer the admin asked for. Only a request that never
   * reached the action is caught here — and it is shown the same way, since
   * from where the admin is sitting the question was the same.
   *
   * A workflow body answers a run id and its state (§10.3) rather than a value,
   * so the toast carries that and the Runs button beside it is where the admin
   * goes next. It does not navigate: doing so would close the toast over the
   * news it was showing. */
  const run = async (trigger: TriggerItem) => {
    setError(null);
    setRan(null);
    setRunning(trigger.id);
    try {
      setRan(testRunOutcome(trigger, await api.testRunTrigger(trigger.id, {})));
    } catch (err) {
      setRan(
        testRunFailure(trigger, errorMessage(err, "The trigger could not be run.")),
      );
    } finally {
      setRunning(null);
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Automation"
        title={t("Triggers")}
        actions={
          <Button onClick={() => navigate("/triggers/new")}>
            <IconPlus className="icon-2" />
            <T text="New trigger" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {ran && <TestRunToast outcome={ran} onClose={() => setRan(null)} />}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Event" /></th>
                <th><T text="Runs" /></th>
                <th><T text="Last run" /></th>
                <th><T text="Status" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {triggers?.length === 0 && (
                <tr>
                  <td colSpan={6} className="text-muted">
                    <T text="No triggers yet." />
                  </td>
                </tr>
              )}
              {triggers?.map((trigger) => (
                <tr key={trigger.id}>
                  <td>
                    {trigger.name}
                    {trigger.description && (
                      <div className="text-muted small">{trigger.description}</div>
                    )}
                  </td>
                  <td>
                    {trigger.when}
                    {trigger.channel && (
                      <div className="text-muted small">
                        {t("on {channel}", { channel: trigger.channel })}
                      </div>
                    )}
                    {scheduleSummary(trigger) && (
                      <div className="text-muted small">{scheduleSummary(trigger)}</div>
                    )}
                    {trigger.only_if && (
                      <div className="text-muted small font-monospace text-break">
                        {t("if {condition}", { condition: trigger.only_if })}
                      </div>
                    )}
                  </td>
                  <td className="text-break">{targetSummary(trigger)}</td>
                  {/* Only a periodic trigger has a schedule to have missed, so
                      it is the only one where "when did this last run?" is a
                      question the list can answer usefully. */}
                  <td className="small">
                    {scheduleSummary(trigger) ? lastRun(trigger) : ""}
                  </td>
                  <td>
                    <StatusCell trigger={trigger} />
                  </td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/triggers/${encodeURIComponent(trigger.id)}/edit`}
                      >
                        <T text="Edit" />
                      </Button>
                      {/* A workflow's steps are not on the trigger form: they
                          are a version of their own, drawn on a canvas, and its
                          runs are durable things an admin comes looking for
                          (§10.3). Both hang off the trigger's id. */}
                      {trigger.body === "workflow" && (
                        <>
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            href={`#/triggers/${encodeURIComponent(trigger.id)}/workflow`}
                          >
                            <T text="Steps" />
                          </Button>
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            href={`#/triggers/${encodeURIComponent(trigger.id)}/runs`}
                          >
                            <T text="Runs" />
                          </Button>
                        </>
                      )}
                      {/* Every trigger, not only a `none` one: a table
                          trigger is run against a row picked at random from its
                          table, and the rest are run against the event they
                          would have had minus what only a real occurrence can
                          supply. A trigger that is broken or switched off is
                          not run — the switch wins however it is asked, and
                          testing an unusable trigger would only re-report the
                          reason already beside it. */}
                      <Button
                        size="sm"
                        variant="outline-primary"
                        disabled={
                          !!trigger.error || !trigger.enabled || running !== null
                        }
                        onClick={() => void run(trigger)}
                      >
                        {running === trigger.id ? (
                          <T text="Running…" />
                        ) : (
                          <T text="Test run" />
                        )}
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(trigger)}
                      >
                        <T text="Delete" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
      </PageBody>
    </>
  );
}

/** What a test run produced: the result or the failure, the row it was given,
 * and the transcript.
 *
 * The transcript is **below** the result and always present when there is one,
 * because the two answer different questions — "what did it return?" and "what
 * did it do?" — and an admin who added a `console.log` is asking the second. A
 * `console.error` is coloured as one: a body that logged an error and then
 * returned a value is a body whose result is not the whole story.
 */
export function TestRunToast({
  outcome,
  onClose,
}: {
  outcome: TestRunOutcome;
  onClose: () => void;
}) {
  return (
    <NoticeToast ok={outcome.ok} title={outcome.title} onClose={onClose}>
      {outcome.row && (
        <div className="text-muted small mb-1">
          <T text="ran on" /> {outcome.row}
        </div>
      )}
      <pre className="mb-0 small text-break text-pre-wrap app-outcome-log">
        {outcome.text}
      </pre>
      {outcome.console.length > 0 && (
        <>
          <div className="text-muted small mt-2 mb-1">
            <T text="Console" />
          </div>
          <pre className="mb-0 small text-break text-pre-wrap app-outcome-log">
            {outcome.console.map((line, n) => (
              <div
                key={n}
                className={line.level === "error" ? "text-danger" : undefined}
              >
                {line.level === "log" ? "" : `${line.level}: `}
                {line.text}
              </div>
            ))}
          </pre>
        </>
      )}
    </NoticeToast>
  );
}

/** Usable, switched off, or broken with the reason — the state that makes a
 * trigger that is not firing fixable instead of merely puzzling. */
function StatusCell({ trigger }: { trigger: TriggerItem }) {
  if (trigger.error) {
    return (
      <>
        <StatusBadge tone="red"><T text="Not usable" /></StatusBadge>
        <div className="text-danger small text-break mt-1">{trigger.error}</div>
      </>
    );
  }
  if (!trigger.enabled) {
    return <StatusBadge tone="secondary"><T text="Off" /></StatusBadge>;
  }
  return <StatusBadge tone="green"><T text="Enabled" /></StatusBadge>;
}
