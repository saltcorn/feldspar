// The Source control screen (APP_SYNC_PROPOSAL §7.5). It is in the sidebar
// whether or not the installation is linked to a repository; until linking
// exists (TODO-app-sync S4, S5) it is the Registry tab alone.
//
// **Registry** (§7.6): every configuration value of the installation, one JSON
// file of the instance tree at a time. The tree on the left lists the files by
// kind, with a filter box; a file that is this instance's alone — a settings
// section that never travels, a build host — carries the mark, and so does a
// file holding such values (a store's directory). The editor is Monaco, which
// checks the JSON against the schema the server gives for the kind where the
// API's types provide one.
//
// **A save is a one-entity deploy against live**, so it is reviewed like one:
// the operations under their headings, what loses data unticked until the
// admin ticks it and types the entry's name, blockers that disable the button.
// The server re-plans under its lock, so a file someone else changed since it
// was opened is refused as stale. After a save the editor shows what is stored,
// and says where that differs from what was typed. New and Delete go through
// the same review.

import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import ListGroup from "react-bootstrap/ListGroup";
import Modal from "react-bootstrap/Modal";
import Nav from "react-bootstrap/Nav";
import Row from "react-bootstrap/Row";

import { api, errorMessage } from "../api";
import type { PlanRegistrySaveResponse } from "../client";
import { CodeEditor } from "../CodeEditor";
import { T, useT } from "../i18n";
import { IconPlus, IconTrash } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  NEW_KINDS,
  canConfirm,
  changeLine,
  classLabel,
  confirmationWord,
  defaultTicks,
  draftText,
  entryLabel,
  groupEntries,
  groupOps,
  instanceOnlyKeys,
  kindLabel,
  newEntryPath,
  newEntryValue,
  parseDraft,
  tickable,
  wholeInstanceOnly,
  type RegistryEntity,
  type RegistryEntry,
  type RegistryReceipt,
} from "../registry";

export function SourceControlPage({ initialPath }: { initialPath?: string }) {
  const { t } = useT();
  return (
    <>
      <PageHeader pretitle={t("Administration")} title={t("Source control")} />
      <PageBody>
        <Nav variant="tabs" className="mb-3" activeKey="registry">
          <Nav.Item>
            <Nav.Link eventKey="registry" href="#/source-control">
              <T text="Registry" />
            </Nav.Link>
          </Nav.Item>
        </Nav>
        <RegistryTab initialPath={initialPath} />
      </PageBody>
    </>
  );
}

/** What a save is of: the file, and its new JSON (`null` deletes it), from
 * the version opened (`null` for a new file). */
interface PendingSave {
  path: string;
  name: string;
  value: unknown | null;
  version: string | null;
}

function RegistryTab({ initialPath }: { initialPath?: string }) {
  const { t } = useT();
  const [entries, setEntries] = useState<RegistryEntry[] | null>(null);
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState<RegistryEntity | null>(null);
  // A new entry being written: not on the server yet.
  const [fresh, setFresh] = useState<{ path: string; name: string } | null>(null);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [review, setReview] = useState<{ save: PendingSave; plan: PlanRegistrySaveResponse } | null>(
    null,
  );
  const [receipt, setReceipt] = useState<RegistryReceipt | null>(null);
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setEntries((await api.listRegistry()).entries);
    } catch (e) {
      setError(errorMessage(e, t("Could not load the registry.")));
    }
  }, [t]);

  const openPath = useCallback(
    async (path: string) => {
      setError(null);
      setReceipt(null);
      setFresh(null);
      try {
        const entity = await api.getRegistryEntity({ path });
        setOpen(entity);
        setText(draftText(entity.value));
        window.history.replaceState(null, "", `#/source-control?path=${encodeURIComponent(path)}`);
      } catch (e) {
        setError(errorMessage(e, t("Could not open {path}.", { path })));
      }
    },
    [t],
  );

  useEffect(() => {
    void load();
    if (initialPath) void openPath(initialPath);
  }, [load, openPath, initialPath]);

  const groups = useMemo(() => groupEntries(entries ?? [], filter, t), [entries, filter, t]);
  const editing = open ?? null;
  const current: PendingSave | null = fresh
    ? { path: fresh.path, name: fresh.name, value: null, version: null }
    : editing
      ? { path: editing.path, name: editing.name, value: null, version: editing.version }
      : null;

  const plan = async (save: PendingSave) => {
    setError(null);
    setBusy(true);
    try {
      const planned = await api.planRegistrySave({
        path: save.path,
        value: save.value,
        version: save.version,
      });
      setReview({ save: { ...save, value: planned.value ?? save.value }, plan: planned });
    } catch (e) {
      setError(errorMessage(e, t("The save could not be planned.")));
    } finally {
      setBusy(false);
    }
  };

  const onSave = () => {
    if (!current) return;
    const parsed = parseDraft(text);
    if (!parsed.ok) {
      setError(t("The JSON does not parse: {error}", { error: parsed.error }));
      return;
    }
    void plan({ ...current, value: parsed.value });
  };

  const onDelete = () => {
    if (!editing) return;
    if (!window.confirm(t("Delete {path}? The review shows what that does.", { path: editing.path })))
      return;
    void plan({ path: editing.path, name: editing.name, value: null, version: editing.version });
  };

  const onConfirmed = async (done: RegistryReceipt) => {
    setReview(null);
    setReceipt(done);
    setFresh(null);
    await load();
    if (done.path) {
      const entity = await api.getRegistryEntity({ path: done.path }).catch(() => null);
      setOpen(entity);
      if (entity) setText(draftText(entity.value));
    } else {
      setOpen(null);
      setText("");
    }
  };

  return (
    <Row className="g-3">
      <Col lg={4}>
        <Card>
          <Card.Header className="d-flex gap-2 align-items-center">
            <Form.Control
              size="sm"
              type="search"
              placeholder={t("Filter")}
              aria-label={t("Filter the registry")}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            <Button size="sm" variant="outline-primary" onClick={() => setCreating(true)}>
              <IconPlus /> <T text="New" />
            </Button>
          </Card.Header>
          <div style={{ maxHeight: "70vh", overflowY: "auto" }} data-testid="registry-tree">
            {entries === null ? (
              <Card.Body className="text-secondary">
                <T text="Loading…" />
              </Card.Body>
            ) : groups.length === 0 ? (
              <Card.Body className="text-secondary">
                <T text="Nothing matches the filter." />
              </Card.Body>
            ) : (
              groups.map((group) => (
                <div key={group.kind}>
                  <div className="px-3 pt-2 pb-1 small fw-bold text-secondary text-uppercase">
                    {kindLabel(group.kind, t)}
                    {group.instanceOnly && <InstanceOnlyBadge className="ms-2" />}
                  </div>
                  <ListGroup variant="flush">
                    {group.entries.map((e) => (
                      <ListGroup.Item
                        key={e.path}
                        action
                        active={current?.path === e.path}
                        onClick={() => void openPath(e.path)}
                        className="d-flex justify-content-between align-items-center py-1"
                        title={e.path}
                      >
                        <span className="text-truncate">{entryLabel(e)}</span>
                        {wholeInstanceOnly(e) && !group.instanceOnly ? (
                          <InstanceOnlyBadge />
                        ) : instanceOnlyKeys(e).length > 0 ? (
                          <StatusBadge tone="secondary" title={instanceOnlyKeys(e).join(", ")}>
                            <T text="some values this instance only" />
                          </StatusBadge>
                        ) : null}
                      </ListGroup.Item>
                    ))}
                  </ListGroup>
                </div>
              ))
            )}
          </div>
        </Card>
      </Col>
      <Col lg={8}>
        {error && (
          <Alert variant="danger" onClose={() => setError(null)} dismissible>
            {error}
          </Alert>
        )}
        {receipt && <SavedNotice receipt={receipt} onClose={() => setReceipt(null)} />}
        {current ? (
          <Card>
            <Card.Header className="d-flex justify-content-between align-items-center gap-2">
              <div className="text-truncate">
                <code>{current.path}</code>
                {fresh && (
                  <StatusBadge tone="blue" className="ms-2">
                    <T text="new" />
                  </StatusBadge>
                )}
                {editing && wholeInstanceOnly(editing) && <InstanceOnlyBadge className="ms-2" />}
              </div>
              <div className="d-flex gap-2">
                {editing && !fresh && (
                  <Button size="sm" variant="outline-danger" onClick={onDelete} disabled={busy}>
                    <IconTrash /> <T text="Delete" />
                  </Button>
                )}
                <Button size="sm" onClick={onSave} disabled={busy}>
                  <T text="Save" />
                </Button>
              </div>
            </Card.Header>
            <Card.Body>
              {editing && wholeInstanceOnly(editing) && (
                <p className="small text-secondary">
                  <T text="This is this instance's alone: it is never written to git, and editing it is never a local change." />
                </p>
              )}
              {editing && instanceOnlyKeys(editing).length > 0 && (
                <p className="small text-secondary">
                  {t("These values are this instance's alone and are never written to git: {keys}", {
                    keys: instanceOnlyKeys(editing).join(", "),
                  })}
                </p>
              )}
              <CodeEditor
                key={current.path}
                id="registry-editor"
                language="json"
                value={text}
                onChange={setText}
                jsonSchema={(editing?.schema as object | null | undefined) ?? null}
                height="60vh"
              />
              <p className="small text-secondary mt-2 mb-0">
                <T text="A secret shows as the mask: leave it to keep the stored value, or type a new one to replace it." />
              </p>
            </Card.Body>
          </Card>
        ) : (
          <Card>
            <Card.Body className="text-secondary">
              <T text="Pick an entry to see and edit its JSON." />
            </Card.Body>
          </Card>
        )}
      </Col>
      {review && (
        <SaveReview
          save={review.save}
          plan={review.plan}
          onCancel={() => setReview(null)}
          onDone={(done) => void onConfirmed(done)}
          onError={(message) => {
            setReview(null);
            setError(message);
          }}
        />
      )}
      {creating && (
        <NewEntry
          entries={entries ?? []}
          onCancel={() => setCreating(false)}
          onCreate={(path, name, value) => {
            setCreating(false);
            setOpen(null);
            setReceipt(null);
            setError(null);
            setFresh({ path, name });
            setText(draftText(value));
          }}
        />
      )}
    </Row>
  );
}

function InstanceOnlyBadge({ className }: { className?: string }) {
  const { t } = useT();
  return (
    <StatusBadge
      tone="yellow"
      className={className}
      title={t("Never written to git: this instance's own configuration")}
    >
      <T text="this instance only" />
    </StatusBadge>
  );
}

/** What a save did: its result, the safety backup, what was deferred, and
 * where what is stored differs from what was typed. */
function SavedNotice({ receipt, onClose }: { receipt: RegistryReceipt; onClose: () => void }) {
  const { t } = useT();
  const failed = receipt.result === "failed";
  return (
    <Alert
      variant={failed ? "danger" : receipt.changes.length > 0 ? "warning" : "success"}
      onClose={onClose}
      dismissible
      data-testid="registry-saved"
    >
      <div className="fw-bold">
        {failed ? <T text="The save failed." /> : receipt.path ? <T text="Saved." /> : <T text="Deleted." />}
      </div>
      {receipt.failed && <div>{receipt.failed.message}</div>}
      {receipt.deferred.length > 0 && (
        <div>{t("Not applied, as not ticked: {ops}", { ops: receipt.deferred.join(", ") })}</div>
      )}
      {receipt.backups.length > 0 && (
        <div>{t("A safety backup was written first: {file}", { file: receipt.backups.join(", ") })}</div>
      )}
      {receipt.changes.length > 0 && (
        <>
          <div>
            <T text="What is stored differs from what was typed, and the editor now shows what is stored:" />
          </div>
          <ul className="mb-0">
            {receipt.changes.map((c) => (
              <li key={c.path}>
                <code>{changeLine(c)}</code>
              </li>
            ))}
          </ul>
        </>
      )}
      {receipt.notes.map((note) => (
        <div key={note} className="small">
          {note}
        </div>
      ))}
    </Alert>
  );
}

/** The review of a save: the operations under their headings with their
 * ticks, the blockers, and the typed confirmation for what loses data. */
function SaveReview({
  save,
  plan,
  onCancel,
  onDone,
  onError,
}: {
  save: PendingSave;
  plan: PlanRegistrySaveResponse;
  onCancel: () => void;
  onDone: (receipt: RegistryReceipt) => void;
  onError: (message: string) => void;
}) {
  const { t } = useT();
  const [ticked, setTicked] = useState(() => defaultTicks(plan.operations));
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const word = confirmationWord(plan.operations, ticked, save.name);

  const toggle = (id: string) => {
    const next = new Set(ticked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setTicked(next);
  };

  const confirm = async () => {
    setBusy(true);
    try {
      onDone(
        await api.registrySave({
          path: save.path,
          value: save.value,
          version: save.version,
          plan_digest: plan.digest,
          ticked: [...ticked],
        }),
      );
    } catch (e) {
      onError(errorMessage(e, t("The save failed.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show onHide={onCancel} size="lg" data-testid="registry-review">
      <Modal.Header closeButton>
        <Modal.Title>{t("Save {path}", { path: save.path })}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {plan.operations.length === 0 && (
          <p className="text-secondary">
            <T text="Nothing would change: what was typed is what is stored." />
          </p>
        )}
        {groupOps(plan.operations).map((group) => (
          <div key={group.cls} className="mb-3">
            <div
              className={`small fw-bold text-uppercase ${
                group.cls === "data_destructive" || group.cls === "unsupported"
                  ? "text-danger"
                  : "text-secondary"
              }`}
            >
              {classLabel(group.cls, t)}
            </div>
            {group.ops.map((op) => (
              <Form.Check
                key={op.id}
                id={`op-${op.id}`}
                type="checkbox"
                label={op.summary}
                checked={ticked.has(op.id)}
                disabled={!tickable(op)}
                onChange={() => toggle(op.id)}
                data-class={op.class}
              />
            ))}
          </div>
        ))}
        {plan.blockers.length > 0 && (
          <Alert variant="danger">
            <div className="fw-bold">
              <T text="This save cannot be made:" />
            </div>
            <ul className="mb-0">
              {plan.blockers.map((b) => (
                <li key={b.message}>{b.message}</li>
              ))}
            </ul>
          </Alert>
        )}
        {plan.needs_secrets.length > 0 && (
          <p className="small text-secondary">
            {t("Created without a secret this instance has not got: {list}", {
              list: plan.needs_secrets.join(", "),
            })}
          </p>
        )}
        {word !== null && (
          <Form.Group>
            <Form.Label>
              {t("This loses data, and a safety backup is written first. Type {word} to confirm.", { word })}
            </Form.Label>
            <Form.Control
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              aria-label={t("Confirmation")}
            />
          </Form.Group>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="link" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button
          onClick={() => void confirm()}
          disabled={busy || !canConfirm(plan, ticked, typed, save.name)}
          variant={word !== null ? "danger" : "primary"}
        >
          <T text="Save" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/** New: a kind, a name (and an application for a kind inside one), and the
 * JSON it starts from. */
function NewEntry({
  entries,
  onCancel,
  onCreate,
}: {
  entries: readonly RegistryEntry[];
  onCancel: () => void;
  onCreate: (path: string, name: string, value: unknown) => void;
}) {
  const { t } = useT();
  const [kind, setKind] = useState(NEW_KINDS[0].kind);
  const [name, setName] = useState("");
  const apps = entries.filter((e) => e.kind === "application").map((e) => e.name);
  const [application, setApplication] = useState(apps[0] ?? "");
  const chosen = NEW_KINDS.find((k) => k.kind === kind) ?? NEW_KINDS[0];
  const path = newEntryPath(chosen, name, application);
  const taken = path !== null && entries.some((e) => e.path === path);

  return (
    <Modal show onHide={onCancel}>
      <Modal.Header closeButton>
        <Modal.Title>
          <T text="New registry entry" />
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <Form.Group className="mb-2">
          <Form.Label>
            <T text="Kind" />
          </Form.Label>
          <Form.Select value={kind} onChange={(e) => setKind(e.target.value)}>
            {NEW_KINDS.map((k) => (
              <option key={k.kind} value={k.kind}>
                {kindLabel(k.kind, t)}
              </option>
            ))}
          </Form.Select>
        </Form.Group>
        {chosen.scoped && (
          <Form.Group className="mb-2">
            <Form.Label>
              <T text="Application" />
            </Form.Label>
            <Form.Select value={application} onChange={(e) => setApplication(e.target.value)}>
              {apps.map((a) => (
                <option key={a} value={a}>
                  {a}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
        )}
        <Form.Group className="mb-2">
          <Form.Label>
            <T text="Name" />
          </Form.Label>
          <Form.Control value={name} onChange={(e) => setName(e.target.value)} autoFocus />
        </Form.Group>
        {path && (
          <p className="small text-secondary mb-0">
            <code>{path}</code>
          </p>
        )}
        {taken && (
          <p className="small text-danger mb-0">
            <T text="There is an entry at this path already." />
          </p>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="link" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button
          disabled={path === null || taken}
          onClick={() => path && onCreate(path, name.trim(), newEntryValue(chosen, name))}
        >
          <T text="Write its JSON" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
