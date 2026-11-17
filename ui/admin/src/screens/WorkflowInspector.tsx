// The workflow editor's inspector: everything about the **selected step** that
// is not its position on the canvas (§10.3, phase 6.4).
//
// The canvas draws control flow; this draws the rest — the step's name, what it
// does, where it goes and what happens when it fails. The two halves meet in one
// `Step` value: the inspector hands a whole edited step back, and the editor
// puts it into the workflow, so nothing here knows about nodes or edges.
//
// The part worth stating is the `Action` editor, because it is the reason this
// file does not grow when someone adds an action: the action picker is the
// server's `listActions`, and the settings below it are `SettingsFields` over
// that action's own `config_spec` — the same component the trigger form renders
// an action body with. A plugin's action gets a working step form with no change
// to this file, which is the same promise §13.3 makes about frameworks and
// file-store backends.
//
// The other four kinds each get their own small editor, because each asks a
// different question and a superset with most of it greyed out is not a form.

import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";

import type { ListActionsResponse } from "../client";
import { IconPlus, IconTrash } from "../icons";
import { OptionalRoleSelect } from "../roleSelect";
import type { Roles } from "../roles";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import {
  KIND_INFO,
  emptyKind,
  type ErrorPolicy,
  type FieldDecl,
  type Next,
  type Step,
  type StepKindName,
  type Workflow,
} from "../workflowGraph";
import { T, useT } from "../i18n";

type ActionInfo = ListActionsResponse[number];

/** The types a `user_form` field can ask for — the set `FieldDecl::to_form_field`
 * recognises, and no more: `bytes` is not a thing a form asks a person for, and
 * declaring one would be a control nobody can fill in. */
const FIELD_TYPES = [
  "text",
  "int",
  "float",
  "decimal",
  "bool",
  "json",
  "uuid",
  "date",
  "time",
  "timestamp",
];

/** The step being edited, and everything it needs to be edited against. */
export function StepInspector({
  step,
  workflow,
  actions,
  roles,
  channel,
  event,
  onChange,
  onDelete,
  onMakeStart,
}: {
  step: Step;
  workflow: Workflow;
  actions: ActionInfo[];
  roles: Roles;
  /** The table the trigger fires on, for an action's table-dependent settings. */
  channel?: string;
  /** The event kind, for what a code setting's editor declares in scope. */
  event: string;
  onChange: (step: Step) => void;
  onDelete: () => void;
  onMakeStart: () => void;
}) {
  const { t } = useT();
  const isStart = workflow.start === step.name;
  const others = workflow.steps.filter((s) => s.name !== step.name).map((s) => s.name);
  const all = workflow.steps.map((s) => s.name);

  return (
    <div className="workflow-inspector">
      <Card className="mb-3">
        <Card.Header className="d-flex align-items-center justify-content-between">
          <span>
            {t("{kind} step", { kind: KIND_INFO[step.kind.type].label })}
            {isStart && <span className="badge bg-green-lt ms-2"><T text="start" /></span>}
          </span>
          <div className="btn-list">
            {!isStart && (
              <Button size="sm" variant="outline-secondary" onClick={onMakeStart}>
                <T text="Start here" />
              </Button>
            )}
            <Button size="sm" variant="outline-danger" onClick={onDelete}>
              <IconTrash className="icon-2" />
            </Button>
          </div>
        </Card.Header>
        <Card.Body>
          <Form.Group className="mb-3" controlId="stepName">
            <Form.Label>
              <T text="Name" /><span className="text-danger"> *</span>
            </Form.Label>
            <Form.Control
              value={step.name}
              onChange={(e) => onChange({ ...step, name: e.target.value })}
            />
            <Form.Text muted>
              <T text="What other steps point at and, for an action step, the context key for its result. Renaming updates everything that uses it." />
            </Form.Text>
          </Form.Group>
          <Form.Group className="mb-3" controlId="stepDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={step.description ?? ""}
              onChange={(e) => onChange({ ...step, description: e.target.value })}
            />
          </Form.Group>
          <Form.Group className="mb-0" controlId="stepKind">
            <Form.Label><T text="Does" /></Form.Label>
            <Form.Select
              value={step.kind.type}
              onChange={(e) =>
                // A different kind is a different question; carrying the old
                // answers over would keep settings the new kind never declared.
                onChange({ ...step, kind: emptyKind(e.target.value as StepKindName) })
              }
            >
              {Object.entries(KIND_INFO).map(([value, info]) => (
                <option key={value} value={value}>
                  {info.label}
                </option>
              ))}
            </Form.Select>
            <Form.Text muted>{KIND_INFO[step.kind.type].hint}</Form.Text>
          </Form.Group>
        </Card.Body>
      </Card>

      <Card className="mb-3">
        <Card.Header>{KIND_INFO[step.kind.type].label}</Card.Header>
        <Card.Body>
          <KindEditor
            step={step}
            actions={actions}
            roles={roles}
            channel={channel}
            event={event}
            steps={all}
            onChange={onChange}
          />
        </Card.Body>
      </Card>

      <Card className="mb-3">
        <Card.Header><T text="Then" /></Card.Header>
        <Card.Body>
          <NextEditor
            next={step.next}
            steps={others}
            onChange={(next) => onChange({ ...step, next })}
          />
        </Card.Body>
      </Card>

      <Card className="mb-3">
        <Card.Header><T text="If it fails" /></Card.Header>
        <Card.Body>
          <ErrorPolicyEditor
            policy={step.error_policy ?? null}
            steps={all}
            inherited={workflow.error_policy ?? { type: "fail" }}
            onChange={(error_policy) => onChange({ ...step, error_policy })}
          />
        </Card.Body>
      </Card>
    </div>
  );
}

/** The editor for whichever of the five kinds this step is. */
function KindEditor({
  step,
  actions,
  roles,
  channel,
  event,
  steps,
  onChange,
}: {
  step: Step;
  actions: ActionInfo[];
  roles: Roles;
  channel?: string;
  event: string;
  steps: string[];
  onChange: (step: Step) => void;
}) {
  const { t } = useT();
  const kind = step.kind;
  const set = (next: Step["kind"]) => onChange({ ...step, kind: next });

  if (kind.type === "action") {
    const action = actions.find((a) => a.name === kind.action);
    const spec = action?.config_spec ?? [];
    const values = readConfig(kind.configuration ?? {});
    return (
      <>
        <Form.Group className="mb-3" controlId="stepAction">
          <Form.Label>
            <T text="Action" /><span className="text-danger"> *</span>
          </Form.Label>
          <Form.Select
            value={kind.action}
            onChange={(e) => set({ type: "action", action: e.target.value, configuration: {} })}
          >
            <option value="">—</option>
            {actions.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
          </Form.Select>
          {action && <Form.Text muted>{action.description}</Form.Text>}
        </Form.Group>
        {/* Rendered from the server's own declaration: this file has no
            knowledge of any particular action's settings. */}
        <SettingsFields
          spec={spec}
          values={values}
          idPrefix="step-cfg"
          codeScope={{ table: channel, event, run: true }}
          onChange={(name, value) =>
            set({
              type: "action",
              action: kind.action,
              configuration: buildConfig(spec, { ...values, [name]: value }),
            })
          }
        />
        {/* An action evaluates its own settings, but in the *step's* scope,
            which the engine hands it — so `context` here means what it means in
            a `Set` (§10.3). Say what is in scope where the admin is typing. */}
        <Form.Text muted>
          <T
            text="Values are formulas over the {event} and the {run}: {examples} — the same {context} a {set} step writes."
            values={{
              event: (
                <strong>
                  <T text="event" />
                </strong>
              ),
              run: (
                <strong>
                  <T text="run" />
                </strong>
              ),
              examples: (
                <>
                  <code>row.title</code>, <code>user.email</code>,{" "}
                  <code>context.total</code>
                </>
              ),
              context: <code>context</code>,
              set: <code>Set</code>,
            }}
          />
        </Form.Text>
      </>
    );
  }

  if (kind.type === "set") {
    return (
      <>
        {kind.assignments.length === 0 && (
          <p className="text-muted small"><T text="This step writes nothing into the context." /></p>
        )}
        {kind.assignments.map((assignment, index) => (
          <Row key={index} className="g-2 mb-2 align-items-start">
            <Col md={4}>
              <Form.Control
                aria-label={`Assignment ${index + 1} key`}
                placeholder={t("context key")}
                value={assignment.target}
                onChange={(e) =>
                  set({
                    type: "set",
                    assignments: kind.assignments.map((a, i) =>
                      i === index ? { ...a, target: e.target.value } : a,
                    ),
                  })
                }
              />
            </Col>
            <Col md={7}>
              <Form.Control
                aria-label={`Assignment ${index + 1} formula`}
                className="font-monospace"
                placeholder={t("formula")}
                value={assignment.formula}
                onChange={(e) =>
                  set({
                    type: "set",
                    assignments: kind.assignments.map((a, i) =>
                      i === index ? { ...a, formula: e.target.value } : a,
                    ),
                  })
                }
              />
            </Col>
            <Col md={1}>
              <Button
                size="sm"
                variant="outline-danger"
                aria-label={`Remove assignment ${index + 1}`}
                onClick={() =>
                  set({
                    type: "set",
                    assignments: kind.assignments.filter((_, i) => i !== index),
                  })
                }
              >
                <IconTrash className="icon-2" />
              </Button>
            </Col>
          </Row>
        ))}
        <Button
          size="sm"
          variant="outline-secondary"
          onClick={() =>
            set({
              type: "set",
              assignments: [...kind.assignments, { target: "", formula: "" }],
            })
          }
        >
          <IconPlus className="icon-2" />
          <T text="Add an assignment" />
        </Button>
        <Form.Text muted className="d-block mt-2">
          <T text="Applied in order, so a later formula may read what an earlier one wrote." />
        </Form.Text>
      </>
    );
  }

  if (kind.type === "for_each") {
    return (
      <>
        <Form.Group className="mb-3" controlId="stepOver">
          <Form.Label>
            <T text="Over" /><span className="text-danger"> *</span>
          </Form.Label>
          <Form.Control
            className="font-monospace"
            placeholder="context.order.lines"
            value={kind.over}
            onChange={(e) => set({ ...kind, over: e.target.value })}
          />
          <Form.Text muted><T text="A formula yielding the collection to loop over." /></Form.Text>
        </Form.Group>
        <Row>
          <Col md={6}>
            <Form.Group className="mb-3" controlId="stepVar">
              <Form.Label>
                <T text="Item name" /><span className="text-danger"> *</span>
              </Form.Label>
              <Form.Control
                value={kind.var}
                onChange={(e) => set({ ...kind, var: e.target.value })}
              />
              <Form.Text muted>
                <T text="The body reads it as" /> <code>context.{kind.var || "item"}</code>.
              </Form.Text>
            </Form.Group>
          </Col>
          <Col md={6}>
            <Form.Group className="mb-3" controlId="stepBody">
              <Form.Label>
                <T text="Body" /><span className="text-danger"> *</span>
              </Form.Label>
              <StepSelect
                value={kind.body}
                steps={steps.filter((s) => s !== step.name)}
                blank="—"
                onChange={(body) => set({ ...kind, body })}
              />
              <Form.Text muted>
                <T text="The first step of the body. A path through it that ends comes back here for the next item." />
              </Form.Text>
            </Form.Group>
          </Col>
        </Row>
      </>
    );
  }

  if (kind.type === "wait") {
    return (
      <Form.Group className="mb-0" controlId="stepUntil">
        <Form.Label>
          <T text="Until" /><span className="text-danger"> *</span>
        </Form.Label>
        <Form.Control
          className="font-monospace"
          placeholder="86400000"
          value={kind.until}
          onChange={(e) => set({ ...kind, until: e.target.value })}
        />
        <Form.Text muted>
          <T text="Milliseconds to wait, or an instant to wait until. Survives restarts." />
        </Form.Text>
      </Form.Group>
    );
  }

  // `user_form`
  return (
    <>
      <Row>
        <Col md={6}>
          <Form.Group className="mb-3" controlId="stepAssignTo">
            <Form.Label>
              <T text="Answers go to" /><span className="text-danger"> *</span>
            </Form.Label>
            <Form.Control
              value={kind.assign_to}
              placeholder={t("approval")}
              onChange={(e) => set({ ...kind, assign_to: e.target.value })}
            />
            <Form.Text muted>
              <T text="The context key the answers are merged in under." />
            </Form.Text>
          </Form.Group>
        </Col>
        <Col md={6}>
          <OptionalRoleSelect
            id="stepMinRole"
            label={t("Who may answer")}
            value={kind.min_role ?? null}
            roles={roles}
            blank="Admin only"
            onChange={(min_role) => set({ ...kind, min_role })}
          >
            <T text="The least privileged role still allowed to answer this form." />
          </OptionalRoleSelect>
        </Col>
      </Row>
      <Form.Group className="mb-3" controlId="stepTimeout">
        <Form.Label><T text="Give up after" /></Form.Label>
        <Form.Control
          className="font-monospace"
          placeholder={t("leave blank to wait indefinitely")}
          value={kind.timeout ?? ""}
          onChange={(e) => set({ ...kind, timeout: e.target.value === "" ? null : e.target.value })}
        />
        <Form.Text muted>
          <T text="When to stop waiting. The run then wakes and its error policy decides, so an abandoned approval can branch instead of waiting forever." />
        </Form.Text>
      </Form.Group>

      <Form.Label>
        <T text="Ask for" /><span className="text-danger"> *</span>
      </Form.Label>
      {kind.fields.length === 0 && (
        <p className="text-muted small"><T text="This form asks for nothing yet." /></p>
      )}
      {kind.fields.map((field, index) => (
        <FormFieldRow
          key={index}
          field={field}
          index={index}
          onChange={(next) =>
            set({ ...kind, fields: kind.fields.map((f, i) => (i === index ? next : f)) })
          }
          onRemove={() => set({ ...kind, fields: kind.fields.filter((_, i) => i !== index) })}
        />
      ))}
      <Button
        size="sm"
        variant="outline-secondary"
        onClick={() =>
          set({ ...kind, fields: [...kind.fields, { name: "", type: "text" }] })
        }
      >
        <IconPlus className="icon-2" />
        <T text="Add a field" />
      </Button>
    </>
  );
}

/** One declared form field — the repeated form decision 7's `UserForm` asks for.
 *
 * The same `FormField` vocabulary every other configurable thing declares its
 * settings in, which is why the approval an admin fills in a day later is
 * rendered by `SettingsFields` with no knowledge that workflows exist. */
function FormFieldRow({
  field,
  index,
  onChange,
  onRemove,
}: {
  field: FieldDecl;
  index: number;
  onChange: (field: FieldDecl) => void;
  onRemove: () => void;
}) {
  const { t } = useT();
  return (
    <div className="border rounded p-2 mb-2">
      <Row className="g-2 align-items-end">
        <Col md={4}>
          <Form.Label className="small mb-1"><T text="Name" /></Form.Label>
          <Form.Control
            size="sm"
            aria-label={`Field ${index + 1} name`}
            value={field.name}
            onChange={(e) => onChange({ ...field, name: e.target.value })}
          />
        </Col>
        <Col md={4}>
          <Form.Label className="small mb-1"><T text="Label" /></Form.Label>
          <Form.Control
            size="sm"
            aria-label={`Field ${index + 1} label`}
            placeholder={field.name}
            value={field.label ?? ""}
            onChange={(e) => onChange({ ...field, label: e.target.value })}
          />
        </Col>
        <Col md={3}>
          <Form.Label className="small mb-1"><T text="Type" /></Form.Label>
          <Form.Select
            size="sm"
            aria-label={`Field ${index + 1} type`}
            value={field.type}
            onChange={(e) => onChange({ ...field, type: e.target.value })}
          >
            {FIELD_TYPES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </Form.Select>
        </Col>
        <Col md={1}>
          <Button
            size="sm"
            variant="outline-danger"
            aria-label={`Remove field ${index + 1}`}
            onClick={onRemove}
          >
            <IconTrash className="icon-2" />
          </Button>
        </Col>
      </Row>
      <div className="d-flex gap-3 mt-2">
        <Form.Check
          type="checkbox"
          id={`field-${index}-required`}
          label={t("Required")}
          checked={field.required ?? false}
          onChange={(e) => onChange({ ...field, required: e.target.checked })}
        />
        <Form.Check
          type="checkbox"
          id={`field-${index}-multiline`}
          label={t("Many lines")}
          checked={field.multiline ?? false}
          onChange={(e) => onChange({ ...field, multiline: e.target.checked })}
        />
      </div>
    </div>
  );
}

/** Where a step goes next: the four variants, and the controls for each.
 *
 * A branch is a picker per arm with the guard beside it — which is exactly what
 * the canvas draws as one labelled edge per arm, so editing either changes the
 * same thing. */
export function NextEditor({
  next,
  steps,
  onChange,
}: {
  next: Next;
  steps: string[];
  onChange: (next: Next) => void;
}) {
  const { t } = useT();
  const changeType = (type: Next["type"]) => {
    if (type === next.type) return;
    switch (type) {
      case "end":
        return onChange({ type: "end" });
      case "step":
        return onChange({ type: "step", step: steps[0] ?? "" });
      case "branch":
        return onChange({ type: "branch", arms: [{ when: "", step: steps[0] ?? "" }] });
      case "formula":
        return onChange({ type: "formula", formula: "" });
    }
  };

  return (
    <>
      <Form.Group className="mb-3" controlId="stepNextType">
        <Form.Select value={next.type} onChange={(e) => changeType(e.target.value as Next["type"])}>
          <option value="end"><T text="End the run" /></option>
          <option value="step"><T text="Go to one step" /></option>
          <option value="branch"><T text="Branch on a condition" /></option>
          <option value="formula"><T text="A formula names the step" /></option>
        </Form.Select>
      </Form.Group>

      {next.type === "step" && (
        <StepSelect
          value={next.step}
          steps={steps}
          blank="—"
          onChange={(step) => onChange({ type: "step", step })}
        />
      )}

      {next.type === "branch" && (
        <>
          {next.arms.map((arm, index) => (
            <Row key={index} className="g-2 mb-2 align-items-center">
              <Col xs={12} md={6}>
                <Form.Control
                  className="font-monospace"
                  aria-label={`Arm ${index + 1} condition`}
                  placeholder={t("context.total > 100")}
                  value={arm.when}
                  onChange={(e) =>
                    onChange({
                      ...next,
                      arms: next.arms.map((a, i) =>
                        i === index ? { ...a, when: e.target.value } : a,
                      ),
                    })
                  }
                />
              </Col>
              <Col xs={9} md={5}>
                <StepSelect
                  value={arm.step}
                  steps={steps}
                  blank="—"
                  onChange={(step) =>
                    onChange({
                      ...next,
                      arms: next.arms.map((a, i) => (i === index ? { ...a, step } : a)),
                    })
                  }
                />
              </Col>
              <Col xs={3} md={1}>
                <Button
                  size="sm"
                  variant="outline-danger"
                  aria-label={`Remove arm ${index + 1}`}
                  onClick={() =>
                    onChange({ ...next, arms: next.arms.filter((_, i) => i !== index) })
                  }
                >
                  <IconTrash className="icon-2" />
                </Button>
              </Col>
            </Row>
          ))}
          <Button
            size="sm"
            variant="outline-secondary"
            className="mb-3"
            onClick={() =>
              onChange({ ...next, arms: [...next.arms, { when: "", step: steps[0] ?? "" }] })
            }
          >
            <IconPlus className="icon-2" />
            <T text="Add an arm" />
          </Button>
          <Form.Group controlId="stepOtherwise">
            <Form.Label><T text="Otherwise" /></Form.Label>
            <StepSelect
              value={next.otherwise ?? ""}
              steps={steps}
              blank="End the run"
              onChange={(step) =>
                onChange({ ...next, otherwise: step === "" ? null : step })
              }
            />
            <Form.Text muted><T text="Arms are tried in order; the first true one wins." /></Form.Text>
          </Form.Group>
        </>
      )}

      {next.type === "formula" && (
        <Form.Group controlId="stepNextFormula">
          <Form.Control
            as="textarea"
            rows={2}
            className="font-monospace"
            value={next.formula}
            placeholder={t("context.total > 100 ? 'approve' : 'ship'")}
            onChange={(e) => onChange({ type: "formula", formula: e.target.value })}
          />
          <Form.Text muted>
            <T text="A formula yielding the" /> <strong><T text="name" /></strong> <T text="of the next step, drawn as a dashed edge to a computed marker, because the run decides it." />
          </Form.Text>
        </Form.Group>
      )}
    </>
  );
}

/** What happens to this step when it fails: the workflow's policy, or its own. */
function ErrorPolicyEditor({
  policy,
  steps,
  inherited,
  onChange,
}: {
  policy: ErrorPolicy | null;
  steps: string[];
  /** The workflow's policy, which is what "inherit" means here — said out loud,
   * because "no per-step policy" is not "no policy" (§10.3, decision 4). */
  inherited: ErrorPolicy;
  onChange: (policy: ErrorPolicy | null) => void;
}) {
  const { t } = useT();
  const kind = policy?.type ?? "inherit";
  const change = (value: string) => {
    switch (value) {
      case "inherit":
        return onChange(null);
      case "retry":
        return onChange({ type: "retry", max: 3 });
      case "handler":
        return onChange({ type: "handler", step: steps[0] ?? "" });
      default:
        return onChange({ type: "fail" });
    }
  };
  return (
    <>
      <Form.Group className="mb-3" controlId="stepPolicy">
        <Form.Select value={kind} onChange={(e) => change(e.target.value)}>
          <option value="inherit">
            {t("Whatever the workflow says ({policy})", {
              policy: policyLabel(inherited),
            })}
          </option>
          <option value="retry"><T text="Retry, then fall through" /></option>
          <option value="handler"><T text="Jump to a step" /></option>
          <option value="fail"><T text="Fail the run" /></option>
        </Form.Select>
      </Form.Group>
      {policy?.type === "retry" && <RetryFields policy={policy} onChange={onChange} />}
      {policy?.type === "handler" && (
        <>
          <StepSelect
            value={policy.step}
            steps={steps}
            blank="—"
            onChange={(step) => onChange({ type: "handler", step })}
          />
          <Form.Text muted>
            <T text="The error arrives in the context under" /> <code>context.error</code>.
          </Form.Text>
        </>
      )}
    </>
  );
}

/** A retry's attempt count and its backoff. */
function RetryFields({
  policy,
  onChange,
}: {
  policy: Extract<ErrorPolicy, { type: "retry" }>;
  onChange: (policy: ErrorPolicy) => void;
}) {
  const { t } = useT();
  const backoff = policy.backoff ?? {
    initial_ms: 1000,
    factor: 2,
    max_ms: 60000,
    jitter: true,
  };
  const set = (over: Partial<typeof backoff>) =>
    onChange({ ...policy, backoff: { ...backoff, ...over } });
  return (
    <Row className="g-2">
      <Col md={3}>
        <Form.Group controlId="retryMax">
          <Form.Label className="small mb-1"><T text="Attempts" /></Form.Label>
          <Form.Control
            type="number"
            min={1}
            value={policy.max}
            onChange={(e) => onChange({ ...policy, max: Number(e.target.value) })}
          />
        </Form.Group>
      </Col>
      <Col md={3}>
        <Form.Group controlId="retryInitial">
          <Form.Label className="small mb-1"><T text="First wait (ms)" /></Form.Label>
          <Form.Control
            type="number"
            min={0}
            value={backoff.initial_ms}
            onChange={(e) => set({ initial_ms: Number(e.target.value) })}
          />
        </Form.Group>
      </Col>
      <Col md={3}>
        <Form.Group controlId="retryFactor">
          <Form.Label className="small mb-1"><T text="Multiply by" /></Form.Label>
          <Form.Control
            type="number"
            step="0.1"
            min={1}
            value={backoff.factor}
            onChange={(e) => set({ factor: Number(e.target.value) })}
          />
        </Form.Group>
      </Col>
      <Col md={3}>
        <Form.Group controlId="retryMaxMs">
          <Form.Label className="small mb-1"><T text="Longest wait (ms)" /></Form.Label>
          <Form.Control
            type="number"
            min={0}
            value={backoff.max_ms}
            onChange={(e) => set({ max_ms: Number(e.target.value) })}
          />
        </Form.Group>
      </Col>
      <Col xs={12}>
        <Form.Check
          type="checkbox"
          id="retryJitter"
          label={t("Spread the waits out randomly")}
          checked={backoff.jitter}
          onChange={(e) => set({ jitter: e.target.checked })}
        />
        <Form.Text muted>
          <T text="On by default: a hundred runs that failed on one outage would otherwise retry at the same instant, which is the outage's second wave." />
        </Form.Text>
      </Col>
    </Row>
  );
}

/** A policy in a few words, for the "inherit" option that has to say what it
 * inherits. */
function policyLabel(policy: ErrorPolicy): string {
  switch (policy.type) {
    case "retry":
      return `retry ${policy.max}×`;
    case "handler":
      return `jump to ${policy.step}`;
    case "fail":
      return "fail the run";
  }
}

/** A select over the workflow's step names. */
function StepSelect({
  value,
  steps,
  blank,
  onChange,
}: {
  value: string;
  steps: string[];
  blank: string;
  onChange: (step: string) => void;
}) {
  const { t } = useT();
  return (
    <Form.Select value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="">{blank}</option>
      {/* A step that was renamed or deleted out from under this reference is
          still shown, so the inspector says what is wrong rather than silently
          reading as "—". Validation names it too. */}
      {value !== "" && !steps.includes(value) && (
        <option value={value}>{t("{name} (missing)", { name: value })}</option>
      )}
      {steps.map((s) => (
        <option key={s} value={s}>
          {s}
        </option>
      ))}
    </Form.Select>
  );
}
