// Create / edit a trigger: one event bound to a **body** — a configured action
// (§10.2) or a workflow (§10.3).
//
// The point this screen proves is the one the framework and file-store pickers
// prove: **no screen knows a specific action's settings**. The admin picks an
// action and the form renders whatever that action's `config_spec` declares, so
// an action added by a plugin gets a working configuration form with no change
// to this file.
//
// The body is the one choice this form makes that is not about settings: an
// action is configured here, and a workflow is a program drawn on its own canvas
// — so choosing "workflow" replaces the settings half with the way to it, and
// saving lands there. The alternative was a second create screen, which would
// duplicate the event half of this one and then have to keep up with it.
//
// What is *not* generic is the event, and deliberately: which fields apply
// depends on the kind. A table event needs a table and can carry an `only_if`
// over the affected row; every other kind has no row, so both are hidden rather
// than offered and then refused on save. The server validates the same rules —
// this form shapes the question, it does not decide the answer, and a save it
// gets wrong comes back with the server's own message.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListActionsResponse, ListTriggersResponse } from "../client";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import { T, useT } from "../i18n";

type ActionInfo = ListActionsResponse[number];
type TriggerItem = ListTriggersResponse[number];

/** The event kinds, in the order the server lists them (`EVENT_KINDS`).
 *
 * Hard-coded because they are a fixed enum of the trigger model rather than a
 * registry: nothing can add one at runtime, which is exactly the difference from
 * the action list beside it (which *is* loaded from the server). The labels are
 * this screen's editorial job. */
const EVENT_KINDS: {
  value: string;
  label: string;
  table: boolean;
  /** The channel names a **stream** rather than a table (§8). One kind, and it
   * is the reason `channel` is not simply "the table": a stream event has a
   * required channel and no row, which is a combination no other kind has. */
  stream?: boolean;
  /** Which timing inputs this kind takes — exactly the attributes the server's
   * `Schedule::of` reads for it. A kind that does not take one *refuses* it on
   * save, so offering it would be offering a field that cannot be saved. */
  timing: TimingField[];
}[] = [
  { value: "insert", label: "A row is inserted", table: true, timing: [] },
  { value: "update", label: "A row is updated", table: true, timing: [] },
  {
    value: "insert_or_update",
    label: "A row is inserted or updated",
    table: true,
    timing: [],
  },
  { value: "delete", label: "A row is deleted", table: true, timing: [] },
  {
    value: "none",
    label: "Only when something asks (no event)",
    table: false,
    timing: [],
  },
  { value: "login", label: "A user signs in", table: false, timing: [] },
  { value: "startup", label: "The server starts up", table: false, timing: [] },
  { value: "error", label: "An error is reported", table: false, timing: [] },
  { value: "often", label: "Every five minutes", table: false, timing: [] },
  { value: "hourly", label: "Once an hour", table: false, timing: ["minute"] },
  { value: "daily", label: "Once a day", table: false, timing: ["hour", "minute"] },
  {
    value: "weekly",
    label: "Once a week",
    table: false,
    timing: ["day_of_week", "hour", "minute"],
  },
  {
    value: "stream",
    label: "An element arrives on a stream",
    table: false,
    stream: true,
    timing: [],
  },
];

/** The three timing inputs a periodic trigger can have. */
type TimingField = "minute" | "hour" | "day_of_week";

/** Days as the server numbers them: 0 = Monday … 6 = Sunday. The admin sees the
 * name, so the convention only has to be consistent. */
const DAYS = [
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
  "Sunday",
];

/** Whether a kind is about a table row — which is what decides whether the table
 * picker and the `only_if` field apply at all. */
function isTableEvent(kind: string): boolean {
  return EVENT_KINDS.find((k) => k.value === kind)?.table ?? false;
}

/** The table an action's *declaration* should be read for, given the event kind
 * and the chosen table — `undefined` where there is none.
 *
 * An action may declare different settings for different tables: `send_email`
 * offers one attachment checkbox per File field of the table the trigger fires
 * on. So the action list is re-read whenever this answer changes, and this
 * screen still knows nothing about any particular setting — it asks for the
 * declarations that apply and renders what comes back.
 *
 * A kind with no row has no table, and a table event with none chosen yet has
 * nothing to ask about: both are `undefined`, which is the table-independent
 * declaration. */
export function actionSpecTable(
  when: string,
  channel: string,
): string | undefined {
  if (!isTableEvent(when)) return undefined;
  const table = channel.trim();
  return table === "" ? undefined : table;
}

/** Whether a kind's channel is a stream — which is what turns the channel box
 * into the stream picker, and what makes `only_if` read the envelope rather
 * than a row (§8). */
export function isStreamEvent(kind: string): boolean {
  return EVENT_KINDS.find((k) => k.value === kind)?.stream ?? false;
}

/** The timing inputs a kind takes; empty for everything that is not periodic. */
function timingFields(kind: string): TimingField[] {
  return EVENT_KINDS.find((k) => k.value === kind)?.timing ?? [];
}

/**
 * The trigger editor, for a new trigger or an existing one.
 *
 * `table` pre-selects the table a new trigger fires on — what "Create trigger"
 * on a table's own page means. It applies only to a new trigger: an existing
 * one's table is its own, and the form reads it from the stored trigger.
 */
export function TriggerForm({ triggerId, table }: { triggerId?: string; table?: string }) {
  const { t } = useT();
  const roles = useRoles();
  const [actions, setActions] = useState<ActionInfo[] | null>(null);
  const [tables, setTables] = useState<string[]>([]);
  /** The streams this server has, for a `stream` event's channel. Empty on a
   * build with no stream support, which makes the picker empty rather than the
   * form broken — and the server refuses an unknown name either way (§8). */
  const [streams, setStreams] = useState<string[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [when, setWhen] = useState("insert");
  const [channel, setChannel] = useState(table ?? "");
  const [onlyIf, setOnlyIf] = useState("");
  // Which engine runs this trigger: `action` (a configured action) or
  // `workflow` (a program). New triggers default to an action, which is what
  // every trigger was before workflows.
  const [body, setBody] = useState<"action" | "workflow">("action");
  const [actionName, setActionName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [minRole, setMinRole] = useState<number | null>(null);
  const [enabled, setEnabled] = useState(true);
  // The periodic timing, held as strings so an empty box stays empty rather than
  // becoming a 0 the admin did not type.
  const [timing, setTiming] = useState<Record<TimingField, string>>({
    minute: "",
    hour: "",
    day_of_week: "",
  });

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const tableList = await api.listTables();
        // Tolerated separately: a server built without stream support answers
        // this with a refusal, and that must not stop a table trigger being
        // edited.
        const streamList = await api.listStreams().catch(() => []);
        let existing: TriggerItem | undefined;
        if (triggerId) {
          existing = (await api.listTriggers()).find((t) => t.id === triggerId);
          if (!existing) {
            if (!cancelled) setLoadError("That trigger no longer exists.");
            return;
          }
        }
        if (cancelled) return;
        setTables(tableList.map((t) => t.name));
        setStreams(streamList.map((stream) => stream.name));
        if (existing) {
          setName(existing.name);
          setDescription(existing.description);
          setWhen(existing.when);
          setChannel(existing.channel ?? "");
          setOnlyIf(existing.only_if ?? "");
          // Both are the action body's, and both come back null for a
          // workflow (§10.3), whose steps are edited on their own screen —
          // this form fills in the fields the two bodies share either way.
          setBody(existing.body === "workflow" ? "workflow" : "action");
          setActionName(existing.action ?? "");
          setConfig(readConfig(existing.configuration ?? {}));
          setMinRole(existing.min_role ?? null);
          setEnabled(existing.enabled);
          setTiming({
            minute: existing.minute == null ? "" : String(existing.minute),
            hour: existing.hour == null ? "" : String(existing.hour),
            day_of_week:
              existing.day_of_week == null ? "" : String(existing.day_of_week),
          });
        }
      } catch {
        if (!cancelled) setLoadError("Could not load the actions and tables.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [triggerId]);

  // The action declarations, re-read whenever the table they may depend on
  // changes (see `actionSpecTable`). Separate from the load above because it has
  // a different trigger: that one runs once for the trigger being edited, this
  // one every time the answer it asks for could have changed.
  const specTable = actionSpecTable(when, channel);
  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const actionList = await api.listActions(
          specTable === undefined ? undefined : { table: specTable },
        );
        if (cancelled) return;
        setActions(actionList);
        // The default for a new trigger, and only ever a default: an action the
        // admin (or the stored trigger) already chose stays chosen.
        setActionName((chosen) => chosen || (actionList[0]?.name ?? ""));
      } catch {
        if (!cancelled) setLoadError("Could not load the actions and tables.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [specTable]);

  const action = actions?.find((a) => a.name === actionName);
  const spec = action?.config_spec ?? [];
  const tableEvent = isTableEvent(when);
  const streamEvent = isStreamEvent(when);
  /** Both kinds that name one, which is what decides the channel box and the
   * `only_if`: a stream event has no row, but it does have a payload. */
  const channelEvent = tableEvent || streamEvent;
  const timingUsed = timingFields(when);

  /** One timing value for the save: null unless this kind uses it *and* the box
   * has something in it. Same rule the table and `only_if` follow — what is not
   * on the screen is not part of the save — and it matters more here, because
   * the server refuses a timing value on a kind that has no use for it. */
  const timingValue = (field: TimingField): number | null =>
    timingUsed.includes(field) && timing[field].trim() !== ""
      ? Number(timing[field])
      : null;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const record = {
        name: name.trim(),
        description: description.trim(),
        when,
        // The fields a non-table event does not have are sent as null rather
        // than as the strings left over from a kind the admin changed away
        // from: what is not on the screen is not part of the save.
        channel: channelEvent && channel !== "" ? channel : null,
        only_if: channelEvent && onlyIf.trim() !== "" ? onlyIf.trim() : null,
        body,
        // Both belong to the **action** body, and both are refused on a workflow
        // — whose steps are a version of their own, edited on the canvas.
        action: body === "action" ? actionName : null,
        configuration: body === "action" ? buildConfig(spec, config) : null,
        min_role: minRole,
        enabled,
        minute: timingValue("minute"),
        hour: timingValue("hour"),
        day_of_week: timingValue("day_of_week"),
      };
      const saved = triggerId
        ? await api.updateTrigger(triggerId, record)
        : await api.createTrigger(record);
      // A workflow trigger is only half-made when the trigger is saved: creating
      // it minted version 1 (an empty canvas, not an error), and the steps are
      // the point. So the save lands on the editor rather than back on a list
      // that would say "workflow" and nothing else.
      navigate(
        saved.body === "workflow"
          ? `/triggers/${encodeURIComponent(saved.id)}/workflow`
          : "/triggers",
      );
    } catch (err) {
      // The server's own refusal — "no table named `books`", "`only if`:
      // unknown identifier `titel`" — is the message that says what to fix.
      setError(errorMessage(err, "Could not save the trigger."));
    } finally {
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!actions) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle="Automation"
        title={triggerId ? "Edit trigger" : "New trigger"}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/triggers")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <Form onSubmit={(e) => void submit(e)}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="triggerName">
                <Form.Label>
                  <T text="Name" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control
                  value={name}
                  required
                  onChange={(e) => setName(e.target.value)}
                />
              </Form.Group>
            </Col>
            <Col md={6}>
              <OptionalRoleSelect
                id="triggerMinRole"
                label={t("Minimum role")}
                value={minRole}
                roles={roles}
                blank="Admin only"
                onChange={setMinRole}
              >
              </OptionalRoleSelect>
            </Col>
          </Row>

          <Form.Group className="mb-3" controlId="triggerDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>

          <Card className="mb-3">
            <Card.Header><T text="When" /></Card.Header>
            <Card.Body>
              <Row>
                <Col md={channelEvent ? 6 : 12}>
                  <Form.Group className="mb-3" controlId="triggerWhen">
                    <Form.Label><T text="Event" /></Form.Label>
                    <Form.Select
                      value={when}
                      onChange={(e) => {
                        const next = e.target.value;
                        // A table name left in the box of a stream event would
                        // be submitted as a stream name and refused — and the
                        // picker would show it as no selection at all, which
                        // reads like the form lost it.
                        if (isStreamEvent(next) !== isStreamEvent(when)) setChannel("");
                        setWhen(next);
                      }}
                    >
                      {EVENT_KINDS.map((kind) => (
                        <option key={kind.value} value={kind.value}>
                          {kind.label}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                {channelEvent && (
                  <Col md={6}>
                    <Form.Group className="mb-3" controlId="triggerChannel">
                      <Form.Label>
                        {streamEvent ? "Stream" : "Table"}
                        <span className="text-danger"> *</span>
                      </Form.Label>
                      <Form.Select
                        value={channel}
                        onChange={(e) => setChannel(e.target.value)}
                      >
                        <option value="">—</option>
                        {(streamEvent ? streams : tables).map((name) => (
                          <option key={name} value={name}>
                            {name}
                          </option>
                        ))}
                      </Form.Select>
                      {streamEvent && streams.length === 0 && (
                        <Form.Text muted>
                          <T text="No streams yet. A stream is created in Streams." />
                        </Form.Text>
                      )}
                    </Form.Group>
                  </Col>
                )}
              </Row>

              {timingUsed.length > 0 && (
                <Row>
                  {timingUsed.includes("day_of_week") && (
                    <Col md={4}>
                      <Form.Group className="mb-3" controlId="triggerDayOfWeek">
                        <Form.Label><T text="Day" /></Form.Label>
                        <Form.Select
                          value={timing.day_of_week}
                          onChange={(e) =>
                            setTiming((t) => ({ ...t, day_of_week: e.target.value }))
                          }
                        >
                          {DAYS.map((day, index) => (
                            <option key={day} value={String(index)}>
                              {day}
                            </option>
                          ))}
                        </Form.Select>
                      </Form.Group>
                    </Col>
                  )}
                  {timingUsed.includes("hour") && (
                    <Col md={4}>
                      <Form.Group className="mb-3" controlId="triggerHour">
                        <Form.Label><T text="Hour (UTC)" /></Form.Label>
                        <Form.Control
                          type="number"
                          min={0}
                          max={23}
                          placeholder="0"
                          value={timing.hour}
                          onChange={(e) =>
                            setTiming((t) => ({ ...t, hour: e.target.value }))
                          }
                        />
                      </Form.Group>
                    </Col>
                  )}
                  {timingUsed.includes("minute") && (
                    <Col md={4}>
                      <Form.Group className="mb-3" controlId="triggerMinute">
                        <Form.Label><T text="Minute past the hour" /></Form.Label>
                        <Form.Control
                          type="number"
                          min={0}
                          max={59}
                          placeholder="0"
                          value={timing.minute}
                          onChange={(e) =>
                            setTiming((t) => ({ ...t, minute: e.target.value }))
                          }
                        />
                      </Form.Group>
                    </Col>
                  )}
                  <Col xs={12}>
                    <Form.Text muted>
                      <T text="Schedules are in" /> <strong><T text="UTC" /></strong><T text=", so they mean the same instant wherever the server runs and are not moved by daylight saving. An empty box is 0." />
                    </Form.Text>
                  </Col>
                </Row>
              )}

              {channelEvent && (
                <Form.Group className="mb-0" controlId="triggerOnlyIf">
                  <Form.Label><T text="Only if" /></Form.Label>
                  <Form.Control
                    as="textarea"
                    rows={2}
                    className="font-monospace"
                    value={onlyIf}
                    placeholder={
                      streamEvent ? "Example: payload.value.temperature > 30" : "Example: pages > 100"
                    }
                    onChange={(e) => setOnlyIf(e.target.value)}
                  />
                  <Form.Text muted>
                    {streamEvent ? (
                      <>
                        <T
                          text="A JavaScript expression over the element’s envelope. An element is not a row, so there is no {row} or {old} here: the element is {value}, and the provider’s own metadata is {source}. Leave blank to always run."
                          values={{
                            row: <code>row</code>,
                            old: <code>old</code>,
                            value: <code>payload.value</code>,
                            source: <code>payload.source</code>,
                          }}
                        />
                      </>
                    ) : (
                      <>
                        <T
                          text="A JavaScript expression over the affected row’s fields, {row}, {old} and {user}. The action runs only when it is true. Leave blank to always run."
                          values={{
                            row: <code>row</code>,
                            old: <code>old</code>,
                            user: <code>user</code>,
                          }}
                        />
                      </>
                    )}
                  </Form.Text>
                </Form.Group>
              )}
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header><T text="Do" /></Card.Header>
            <Card.Body>
              <Form.Group className="mb-3" controlId="triggerBody">
                <Form.Label><T text="Runs" /></Form.Label>
                <Form.Select
                  value={body}
                  onChange={(e) => setBody(e.target.value as "action" | "workflow")}
                >
                  <option value="action"><T text="One action" /></option>
                  <option value="workflow"><T text="A workflow" /></option>
                </Form.Select>
                <Form.Text muted>
                  <T text="A workflow is a graphical representation of a durable program" />
                </Form.Text>
              </Form.Group>

              {body === "workflow" ? (
                <div className="text-muted">
                  {triggerId ? (
                    <>
                      <T text="The steps live on the canvas." />{" "}
                      <Button
                        size="sm"
                        variant="outline-primary"
                        href={`#/triggers/${encodeURIComponent(triggerId)}/workflow`}
                      >
                        <T text="Open the workflow editor" />
                      </Button>
                    </>
                  ) : (
                    t("Saving opens the editor, on an empty canvas with one start step.")
                  )}
                </div>
              ) : (
                <>
                  <Form.Group className="mb-3" controlId="triggerAction">
                    <Form.Label><T text="Action" /></Form.Label>
                    <Form.Select
                      value={actionName}
                      onChange={(e) => {
                        setActionName(e.target.value);
                        // A different action has different settings; carrying the old
                        // values over would post settings the new one never declared.
                        setConfig({});
                      }}
                    >
                      {actions.map((a) => (
                        <option key={a.name} value={a.name}>
                          {a.name}
                        </option>
                      ))}
                    </Form.Select>
                    {action && <Form.Text muted>{action.description}</Form.Text>}
                  </Form.Group>

                  {/* Rendered from the server's `config_spec`: this screen has no
                  knowledge of any particular action's settings. */}
                  <SettingsFields
                    spec={spec}
                    values={config}
                    idPrefix="trigger-cfg"
                    // What a *code* setting's editor declares: `row` and `old` exist
                    // exactly where this event has them, which is the same rule the
                    // sandbox binds by. The screen knows the event; the setting knows
                    // it is code; neither knows both, so this is where they meet.
                    codeScope={{ table: specTable, event: when }}
                    onChange={(field, value) =>
                      setConfig((prev) => ({ ...prev, [field]: value }))
                    }
                  />
                  <Form.Text muted>
                    <T text="Values are formulas over the event:" /> <code>row.title</code>,{" "}
                    <code>user.email</code>, <code>payload.n</code>.
                  </Form.Text>
                </>
              )}
            </Card.Body>
          </Card>

          <Form.Check
            type="checkbox"
            id="triggerEnabled"
            className="mb-3"
            label={t("Enabled")}
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />

          <div className="d-flex gap-2">
            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : "Save"}
            </Button>
            <Button
              variant="outline-secondary"
              disabled={busy}
              onClick={() => navigate("/triggers")}
            >
              <T text="Cancel" />
            </Button>
          </div>
        </Form>
      </PageBody>
    </>
  );
}
