// The Source control screen (APP_SYNC_PROPOSAL §7.5). It is in the sidebar
// whether or not the installation is linked to a repository. Unlinked, it is
// Registry and Link setup; linked, Status and History come first. Status,
// History, Link setup and the deploy review are `SyncPanels.tsx`.
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
import type { GetSyncStatusResponse, PlanRegistrySaveResponse } from "../client";
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
import {
  classNote,
  diffLine,
  groupLabel,
  jsonChanges,
  statusGroup,
  withoutPointers,
} from "../structureSync";
import { HistoryTab, LinkTab, StatusTab, ZipTab } from "./SyncPanels";

type Tab = "status" | "history" | "registry" | "zip" | "link";

export function SourceControlPage({
  initialPath,
  initialTab,
  merge,
}: {
  initialPath?: string;
  initialTab?: string;
  merge?: boolean;
}) {
  const { t } = useT();
  const [status, setStatus] = useState<GetSyncStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab | null>(
    initialPath ? "registry" : (initialTab as Tab | undefined) ?? null,
  );
  // What the registry is to open: a path from Status, or a version from
  // History to save back.
  const [target, setTarget] = useState<{ path?: string; merge?: boolean; restore?: unknown } | null>(
    initialPath ? { path: initialPath, merge } : null,
  );

  const load = useCallback(async () => {
    try {
      setStatus(await api.getSyncStatus());
      setError(null);
    } catch (e) {
      setError(errorMessage(e, t("Could not load Status.")));
    }
  }, [t]);
  useEffect(() => void load(), [load]);

  const linked = status?.linked ?? false;
  const tabs: Tab[] = linked ? ["status", "history", "registry", "zip", "link"] : ["registry", "zip", "link"];
  const shown: Tab = tab && tabs.includes(tab) ? tab : tabs[0];
  const label = (k: Tab) =>
    k === "status"
      ? t("Status")
      : k === "history"
        ? t("History")
        : k === "registry"
          ? t("Registry")
          : k === "zip"
            ? t("Deploy a zip")
            : t("Link setup");

  return (
    <>
      <PageHeader pretitle={t("Administration")} title={t("Source control")} />
      <PageBody>
        <Nav variant="tabs" className="mb-3" activeKey={shown}>
          {tabs.map((k) => (
            <Nav.Item key={k}>
              {/* A tab is a route of its own (the page is keyed by it), so it
                  can be linked to and survives a reload. */}
              <Nav.Link eventKey={k} href={`#/source-control?tab=${k}`} data-testid={`sync-tab-${k}`}>
                {label(k)}
                {k === "status" && status && status.entries.length > 0 && (
                  <StatusBadge tone="blue" className="ms-2">
                    {status.entries.length}
                  </StatusBadge>
                )}
              </Nav.Link>
            </Nav.Item>
          ))}
        </Nav>
        {error && <Alert variant="danger">{error}</Alert>}
        {shown === "status" && status && (
          <StatusTab
            status={status}
            onReload={load}
            onOpenRegistry={(path, m) => {
              setTarget({ path, merge: m });
              setTab("registry");
            }}
          />
        )}
        {shown === "history" && (
          <HistoryTab
            onRestore={(path, value) => {
              setTarget({ path, restore: value });
              setTab("registry");
            }}
          />
        )}
        {shown === "registry" && (
          <RegistryTab
            key={JSON.stringify(target)}
            initialPath={target?.path}
            merge={target?.merge}
            restore={target?.restore}
            onSaved={load}
          />
        )}
        {shown === "zip" && <ZipTab linked={linked} onDeployed={load} />}
        {shown === "link" && <LinkTab onLinked={load} />}
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

/** Which view of the open entry the editor card shows. */
type EditorView = "json" | "base" | "tip" | "merge";

function RegistryTab({
  initialPath,
  merge,
  restore,
  onSaved,
}: {
  initialPath?: string;
  merge?: boolean;
  restore?: unknown;
  onSaved?: () => Promise<void>;
}) {
  const { t } = useT();
  const [view, setView] = useState<EditorView>(merge ? "merge" : "json");
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
    async (path: string, draft?: unknown) => {
      setError(null);
      setReceipt(null);
      setFresh(null);
      try {
        const entity = await api.getRegistryEntity({ path });
        setOpen(entity);
        setText(draftText(draft === undefined ? entity.value : draft));
        // Merge is for an entity in conflict; another entry opens as JSON.
        setView((v) => (v === "merge" && entity.sync?.class !== "conflict" ? "json" : v));
        window.history.replaceState(null, "", `#/source-control?path=${encodeURIComponent(path)}`);
      } catch (e) {
        setError(errorMessage(e, t("Could not open {path}.", { path })));
      }
    },
    [t],
  );

  useEffect(() => {
    void load();
    // "Restore this version" opens the file with the version to restore in
    // the editor, and saving it is an ordinary save, reviewed as one.
    if (initialPath) void openPath(initialPath, restore);
  }, [load, openPath, initialPath, restore]);

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
    await onSaved?.();
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
              {editing?.sync && !fresh && (
                <Nav variant="pills" className="mb-2 small" activeKey={view}>
                  {(
                    [
                      ["json", t("JSON")],
                      ["base", t("Against the base")],
                      ["tip", t("Against the branch tip")],
                      ...(editing.sync.class === "conflict" || merge ? [["merge", t("Merge")]] : []),
                    ] as [EditorView, string][]
                  ).map(([k, l]) => (
                    <Nav.Item key={k}>
                      <Nav.Link
                        as="button"
                        type="button"
                        className={view === k ? undefined : "bg-transparent"}
                        eventKey={k}
                        onClick={() => setView(k)}
                        data-testid={`registry-view-${k}`}
                      >
                        {l}
                      </Nav.Link>
                    </Nav.Item>
                  ))}
                  {editing.sync.class && editing.sync.class !== "in_sync" && (
                    <span className="ms-auto align-self-center text-secondary">
                      {groupLabel(statusGroup(editing.sync.class), t)} · {classNote(editing.sync.class, t)}
                    </span>
                  )}
                </Nav>
              )}
              {editing?.sync && (view === "base" || view === "tip") && (
                <SideAgainst
                  side={view}
                  other={view === "base" ? editing.sync.base : editing.sync.tip}
                  text={text}
                  instanceOnly={instanceOnlyKeys(editing)}
                />
              )}
              {editing?.sync && view === "merge" && (
                <MergeView
                  base={editing.sync.base}
                  tip={editing.sync.tip}
                  text={text}
                  instanceOnly={instanceOnlyKeys(editing)}
                  onTakeTip={() => setText(draftText(editing.sync?.tip ?? null))}
                />
              )}
              {(view === "json" || view === "merge" || !editing?.sync) && (
                <CodeEditor
                  key={current.path}
                  id="registry-editor"
                  language="json"
                  value={text}
                  onChange={setText}
                  jsonSchema={(editing?.schema as object | null | undefined) ?? null}
                  height={view === "merge" ? "40vh" : "60vh"}
                />
              )}
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

/** What is typed, compared with the base's or the tip's version (this
 * instance's own values left out of both, since they never travel). */
function SideAgainst({
  side,
  other,
  text,
  instanceOnly,
}: {
  side: "base" | "tip";
  other: unknown;
  text: string;
  instanceOnly: readonly string[];
}) {
  const { t } = useT();
  const parsed = parseDraft(text);
  if (!parsed.ok) return <Alert variant="warning">{t("The JSON does not parse: {error}", { error: parsed.error })}</Alert>;
  if (other === null || other === undefined)
    return (
      <p className="text-secondary small" data-testid="registry-against">
        {side === "base" ? t("The base has no such file.") : t("The branch tip has no such file.")}
      </p>
    );
  const changes = jsonChanges(other, withoutPointers(parsed.value, instanceOnly));
  return (
    <div data-testid="registry-against">
      {changes.length === 0 ? (
        <p className="text-secondary small">
          {side === "base" ? t("What is typed is the same as the base.") : t("What is typed is the same as the branch tip.")}
        </p>
      ) : (
        <>
          <p className="small text-secondary mb-1">
            {side === "base" ? t("From the base to what is typed:") : t("From the branch tip to what is typed:")}
          </p>
          <ul className="small" style={{ maxHeight: "60vh", overflow: "auto" }}>
            {changes.map((c) => (
              <li key={c.path}>
                <code>{diffLine(c)}</code>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

/** Merge in editor (§4): the base, the branch's version and this instance's
 * side by side, over the editor that holds the merged result. */
function MergeView({
  base,
  tip,
  text,
  instanceOnly,
  onTakeTip,
}: {
  base: unknown;
  tip: unknown;
  text: string;
  instanceOnly: readonly string[];
  onTakeTip: () => void;
}) {
  const { t } = useT();
  const parsed = parseDraft(text);
  const mine = parsed.ok ? withoutPointers(parsed.value, instanceOnly) : null;
  const pane = (title: string, value: unknown, testid: string) => (
    <Col md={4}>
      <div className="small fw-bold text-secondary">{title}</div>
      <pre
        className="small border rounded p-2 mb-0"
        style={{ maxHeight: "22vh", overflow: "auto" }}
        data-testid={testid}
      >
        {value === null || value === undefined ? t("(none)") : JSON.stringify(value, null, 2)}
      </pre>
    </Col>
  );
  return (
    <div className="mb-2" data-testid="registry-merge">
      <Row className="g-2 mb-2">
        {pane(t("Base"), base, "merge-base")}
        {pane(t("The branch (theirs)"), tip, "merge-tip")}
        {pane(t("Here (mine)"), mine, "merge-mine")}
      </Row>
      <div className="d-flex gap-2 align-items-center mb-2">
        <Button size="sm" variant="outline-secondary" onClick={onTakeTip}>
          <T text="Start from the branch's version" />
        </Button>
        <span className="small text-secondary">
          <T text="Edit the result below and save it. Then commit it from Status with the conflict ticked, which keeps it over the branch's." />
        </span>
      </div>
    </div>
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
