// Source control's tabs besides the registry (APP_SYNC_PROPOSAL §7.5):
//
// - **Status**: the three-way comparison grouped as §4 groups it, each row
//   expandable to its diff, with tick boxes for Commit and Discard, the branch
//   selector, Pull, the commit message, and Deploy — refused with a sentence
//   while anything here is uncommitted (D5).
// - **Deploy's review** (§6 step 5): the operations under their headings with
//   counts, what loses data unticked until the admin ticks it and types the
//   branch's name, blockers that disable the button, then the progress and the
//   receipt. A discard is reviewed the same way, since it is classified the
//   same way (§5).
// - **History**: the ledger, with "restore this version" on a registry save.
// - **Link**: URL, branch, subdirectory, credential (generate a key or paste a
//   token), policy, and a test-connection button.
// - **Deploy a zip** (§7.4): upload a structure zip and deploy it through the
//   same review — in the branch tip's place on a linked instance, and
//   additively (nothing deleted) on one with no link.
//
// The model is `structureSync.ts`.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import ListGroup from "react-bootstrap/ListGroup";
import Modal from "react-bootstrap/Modal";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage, uploadStructureZip, type UploadedStructureZip } from "../api";
import { T, useT } from "../i18n";
import { IconArrowDown, IconChevronDown, IconChevronRight, IconGitBranch, IconRefresh } from "../icons";
import { StatusBadge, type Tone } from "../layout";
import { classLabel, confirmationWord, groupOps, type PlannedOp } from "../registry";
import {
  classCounts,
  classNote,
  commitBody,
  defaultSelection,
  deployRefusal,
  diffLine,
  entryTitle,
  groupLabel,
  groupStatus,
  ledgerKindLabel,
  ledgerSummary,
  linkBody,
  linkForm,
  linkProblem,
  receiptTone,
  restorable,
  reviewCanConfirm,
  reviewTickable,
  reviewTicks,
  selectable,
  shortCommit,
  sideChanged,
  type Deployment,
  type EntityDiff,
  type LinkForm,
  type Policy,
  type Receipt,
  type SideDiff,
  type StatusEntry,
  type StatusGroup,
  type SyncLinkInfo,
  type SyncStatus,
} from "../structureSync";

const GROUP_TONE: Record<StatusGroup, Tone> = {
  conflict: "red",
  local: "yellow",
  incoming: "blue",
  incoming_delete: "secondary",
};

// --- Status ------------------------------------------------------------------------

export function StatusTab({
  status,
  onReload,
  onOpenRegistry,
}: {
  status: SyncStatus;
  onReload: () => Promise<void>;
  onOpenRegistry: (path: string, merge: boolean) => void;
}) {
  const { t } = useT();
  const [selected, setSelected] = useState(() => defaultSelection(status.entries));
  const [expanded, setExpanded] = useState<string | null>(null);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [deploying, setDeploying] = useState(false);
  const [discarding, setDiscarding] = useState<string[] | null>(null);

  useEffect(() => setSelected(defaultSelection(status.entries)), [status]);

  const run = async (what: string, f: () => Promise<string | null>) => {
    setBusy(what);
    setError(null);
    setNotice(null);
    try {
      setNotice(await f());
      await onReload();
    } catch (e) {
      setError(errorMessage(e, t("That did not work.")));
    } finally {
      setBusy(null);
    }
  };

  const toggle = (key: string) => {
    const next = new Set(selected);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    setSelected(next);
  };

  const commit = commitBody(message, status.entries, selected);
  const ticked = status.entries.filter((e) => selectable(e) && selected.has(e.key)).map((e) => e.key);
  const refusal = deployRefusal(status, t);
  const branch = status.link?.branch ?? "";

  return (
    <>
      <Card className="mb-3">
        <Card.Body className="d-flex flex-wrap gap-3 align-items-center">
          <div className="text-truncate" title={status.link?.url}>
            <IconGitBranch /> <code>{status.link?.url}</code>
            {status.link?.subdirectory && <code>/{status.link.subdirectory}</code>}
          </div>
          <BranchSelector current={branch} onSwitched={onReload} />
          <div className="small text-secondary" data-testid="sync-tip">
            {t("Branch tip {tip}, base {base}", {
              tip: shortCommit(status.tip),
              base: shortCommit(status.base_commit),
            })}
          </div>
          <div className="ms-auto d-flex gap-2">
            <Button
              size="sm"
              variant="outline-secondary"
              disabled={busy !== null}
              onClick={() =>
                void run("pull", async () => {
                  const pulled = await api.syncPull();
                  if (pulled.before === pulled.tip) return t("The branch is where it was.");
                  return pulled.forced
                    ? t("The remote was force-pushed; the branch is reset to {tip}.", { tip: shortCommit(pulled.tip) })
                    : t("Pulled {tip}.", { tip: shortCommit(pulled.tip) });
                })
              }
            >
              <IconArrowDown /> {busy === "pull" ? t("Pulling…") : t("Pull")}
            </Button>
            <Button
              size="sm"
              disabled={busy !== null || refusal !== null}
              title={refusal ?? undefined}
              onClick={() => setDeploying(true)}
              data-testid="sync-deploy"
            >
              <T text="Deploy" />
            </Button>
          </div>
        </Card.Body>
      </Card>

      {error && (
        <Alert variant="danger" onClose={() => setError(null)} dismissible>
          {error}
        </Alert>
      )}
      {notice && (
        <Alert variant="success" onClose={() => setNotice(null)} dismissible>
          {notice}
        </Alert>
      )}
      {refusal && <Alert variant="warning">{refusal}</Alert>}
      {status.warnings.length > 0 && (
        <Alert variant="warning" data-testid="sync-warnings">
          <ul className="mb-0">
            {status.warnings.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
        </Alert>
      )}

      {status.entries.length === 0 ? (
        <Card className="mb-3">
          <Card.Body className="text-secondary">
            <T text="Everything here is the same as the branch." />
          </Card.Body>
        </Card>
      ) : (
        groupStatus(status.entries).map(({ group, entries }) => (
          <Card key={group} className="mb-3" data-testid={`sync-group-${group}`}>
            <Card.Header className="d-flex align-items-center gap-2">
              <StatusBadge tone={GROUP_TONE[group]}>{groupLabel(group, t)}</StatusBadge>
              <span className="small text-secondary">{entries.length}</span>
              {group === "conflict" && (
                <span className="small text-secondary">
                  <T text="Tick one to keep mine when committing, or to discard mine; or merge it in the editor." />
                </span>
              )}
            </Card.Header>
            <ListGroup variant="flush">
              {entries.map((e) => (
                <StatusRow
                  key={e.key}
                  entry={e}
                  ticked={selected.has(e.key)}
                  onToggle={() => toggle(e.key)}
                  expanded={expanded === e.key}
                  onExpand={() => setExpanded(expanded === e.key ? null : e.key)}
                  onOpenRegistry={onOpenRegistry}
                />
              ))}
            </ListGroup>
          </Card>
        ))
      )}

      {status.entries.some(selectable) && (
        <Card className="mb-3">
          <Card.Body>
            <Form.Group className="mb-2">
              <Form.Label>
                <T text="Commit message" />
              </Form.Label>
              <Form.Control
                value={message}
                onChange={(e) => setMessage(e.target.value)}
                placeholder={t("What changed, and why")}
                data-testid="sync-message"
              />
            </Form.Group>
            <div className="d-flex gap-2">
              <Button
                disabled={busy !== null || commit === null || !message.trim()}
                onClick={() =>
                  commit &&
                  void run("commit", async () => {
                    const done = await api.syncCommit(commit);
                    setMessage("");
                    return [
                      t("Committed {commit} to {branch}.", { commit: shortCommit(done.commit), branch: done.branch }),
                      ...done.warnings,
                    ].join(" ");
                  })
                }
                data-testid="sync-commit"
              >
                {busy === "commit" ? t("Committing…") : t("Commit and push ticked ({count})", { count: ticked.length })}
              </Button>
              <Button
                variant="outline-danger"
                disabled={busy !== null || ticked.length === 0}
                onClick={() => setDiscarding(ticked)}
              >
                {t("Discard ticked ({count})", { count: ticked.length })}
              </Button>
            </div>
          </Card.Body>
        </Card>
      )}

      {deploying && (
        <Review
          title={t("Deploy {tip} from {branch}", { tip: shortCommit(status.tip), branch })}
          word={branch}
          plan={async () => {
            const plan = await api.planSyncDeploy();
            return { plan, policy: plan.policy, commit: plan.commit ?? null };
          }}
          apply={(plan, ticked, fullBackup, commit) =>
            api.syncDeploy({ commit, plan_digest: plan.digest, ticked: [...ticked], full_backup: fullBackup })
          }
          confirmLabel={t("Deploy")}
          onClose={() => {
            setDeploying(false);
            void onReload();
          }}
        />
      )}
      {discarding && (
        <Review
          title={t("Discard ticked changes ({count})", { count: discarding.length })}
          word={t("discard")}
          plan={async () => ({
            plan: await api.planSyncDiscard({ keys: discarding }),
            policy: status.link?.policy ?? null,
            commit: null,
          })}
          apply={(plan, ticked) =>
            api.syncDiscard({ keys: discarding, plan_digest: plan.digest, ticked: [...ticked] })
          }
          confirmLabel={t("Discard")}
          onClose={() => {
            setDiscarding(null);
            void onReload();
          }}
        />
      )}
    </>
  );
}

function StatusRow({
  entry,
  ticked,
  onToggle,
  expanded,
  onExpand,
  onOpenRegistry,
}: {
  entry: StatusEntry;
  ticked: boolean;
  onToggle: () => void;
  expanded: boolean;
  onExpand: () => void;
  onOpenRegistry: (path: string, merge: boolean) => void;
}) {
  const { t } = useT();
  return (
    <ListGroup.Item data-testid={`sync-entry-${entry.key}`} data-class={entry.class}>
      <div className="d-flex align-items-center gap-2">
        {selectable(entry) ? (
          <Form.Check
            type="checkbox"
            checked={ticked}
            onChange={onToggle}
            aria-label={t("Tick {key}", { key: entry.key })}
          />
        ) : (
          <span style={{ width: "1rem" }} />
        )}
        <Button variant="link" size="sm" className="p-0 text-reset" onClick={onExpand} aria-expanded={expanded}>
          {expanded ? <IconChevronDown /> : <IconChevronRight />}
        </Button>
        <span className="fw-medium">{entryTitle(entry)}</span>
        <span className="small text-secondary">{entry.kind}</span>
        <span className="small text-secondary">· {classNote(entry.class, t)}</span>
        <div className="ms-auto d-flex gap-2">
          {entry.class === "conflict" && entry.in_registry && (
            <Button size="sm" variant="outline-secondary" onClick={() => onOpenRegistry(entry.path, true)}>
              <T text="Merge in editor" />
            </Button>
          )}
          {entry.in_registry && entry.class !== "incoming_new" && entry.class !== "local_delete" && (
            <Button size="sm" variant="link" onClick={() => onOpenRegistry(entry.path, false)}>
              <T text="Open in the registry" />
            </Button>
          )}
        </div>
      </div>
      {expanded && <EntityDiffView entryKey={entry.key} />}
    </ListGroup.Item>
  );
}

/** One entry's diff: what the branch changed from the base, and what this
 * instance did. */
function EntityDiffView({ entryKey }: { entryKey: string }) {
  const { t } = useT();
  const [diff, setDiff] = useState<EntityDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .getSyncEntityDiff({ key: entryKey })
      .then(setDiff)
      .catch((e: unknown) => setError(errorMessage(e, t("Could not load the diff."))));
  }, [entryKey, t]);
  if (error) return <div className="text-danger small mt-2">{error}</div>;
  if (!diff)
    return (
      <div className="mt-2">
        <Spinner size="sm" />
      </div>
    );
  return (
    <Row className="mt-2 g-2" data-testid="sync-diff">
      <Col md={6}>
        <SideView title={t("On the branch, since the base")} side={diff.incoming} />
      </Col>
      <Col md={6}>
        <SideView title={t("Here, since the base")} side={diff.local} />
      </Col>
    </Row>
  );
}

function SideView({ title, side }: { title: string; side: SideDiff }) {
  return (
    <div className="border rounded p-2 h-100">
      <div className="small fw-bold text-secondary mb-1">{title}</div>
      {!sideChanged(side) ? (
        <div className="small text-secondary">
          <T text="No change." />
        </div>
      ) : side.binary ? (
        <div className="small">
          <T text="A binary file changed." />
        </div>
      ) : side.text ? (
        <pre className="small mb-0" style={{ maxHeight: "30vh", overflow: "auto" }}>
          {side.text.unified}
        </pre>
      ) : (
        <ul className="small mb-0 ps-3" style={{ maxHeight: "30vh", overflow: "auto" }}>
          {side.changes.map((c) => (
            <li key={c.path}>
              <code>{diffLine(c)}</code>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function BranchSelector({ current, onSwitched }: { current: string; onSwitched: () => Promise<void> }) {
  const { t } = useT();
  const [branches, setBranches] = useState<string[] | null>(null);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setBranches((await api.listSyncBranches()).branches);
    } catch (e) {
      setError(errorMessage(e, t("Could not list the branches.")));
    }
  }, [t]);
  useEffect(() => void load(), [load, current]);

  const go = async (branch: string, create: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.switchSyncBranch({ name: branch, create });
      setCreating(false);
      setName("");
      await onSwitched();
      await load();
    } catch (e) {
      setError(errorMessage(e, t("Could not switch branch.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="d-flex gap-2 align-items-center">
      {creating ? (
        <>
          <Form.Control
            size="sm"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={t("New branch name")}
            aria-label={t("New branch name")}
            style={{ width: "12rem" }}
          />
          <Button size="sm" disabled={busy || !name.trim()} onClick={() => void go(name.trim(), true)}>
            <T text="Create from the tip" />
          </Button>
          <Button size="sm" variant="link" onClick={() => setCreating(false)}>
            <T text="Cancel" />
          </Button>
        </>
      ) : (
        <Form.Select
          size="sm"
          value={current}
          disabled={busy}
          aria-label={t("Branch")}
          onChange={(e) => {
            if (e.target.value === "\u0000new") setCreating(true);
            else void go(e.target.value, false);
          }}
          style={{ width: "auto" }}
        >
          {(branches ?? [current]).includes(current) ? null : <option value={current}>{current}</option>}
          {(branches ?? [current]).map((b) => (
            <option key={b} value={b}>
              {b}
            </option>
          ))}
          <option value={"\u0000new"}>{t("New branch…")}</option>
        </Form.Select>
      )}
      {error && <span className="small text-danger">{error}</span>}
    </div>
  );
}

// --- The review ------------------------------------------------------------------------

interface Planned {
  plan: {
    operations: PlannedOp[];
    blockers: { op?: string | null; entity?: string | null; message: string }[];
    needs_secrets: string[];
    needs_credentials: string[];
    digest: string;
  };
  policy: Policy | null;
  commit: string | null;
}

/** A deploy's or a discard's review, its progress and its receipt. */
function Review({
  title,
  word,
  plan: makePlan,
  apply,
  confirmLabel,
  onClose,
}: {
  title: string;
  word: string;
  plan: () => Promise<Planned>;
  apply: (plan: Planned["plan"], ticked: Set<string>, fullBackup: boolean, commit: string | null) => Promise<Receipt>;
  confirmLabel: string;
  onClose: () => void;
}) {
  const { t } = useT();
  const [planned, setPlanned] = useState<Planned | null>(null);
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [typed, setTyped] = useState("");
  const [fullBackup, setFullBackup] = useState(false);
  const [applying, setApplying] = useState(false);
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    makePlan()
      .then((p) => {
        setPlanned(p);
        setTicked(reviewTicks(p.plan.operations, p.policy));
      })
      .catch((e: unknown) => setError(errorMessage(e, t("The plan could not be made."))));
    // The plan is made once, when the review opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const toggle = (id: string) => {
    const next = new Set(ticked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setTicked(next);
  };

  const confirm = async () => {
    if (!planned) return;
    setApplying(true);
    setError(null);
    try {
      setReceipt(await apply(planned.plan, ticked, fullBackup, planned.commit));
    } catch (e) {
      setError(errorMessage(e, t("Nothing was applied.")));
    } finally {
      setApplying(false);
    }
  };

  const ops = planned?.plan.operations ?? [];
  const asked = confirmationWord(ops, ticked, word);
  const counts = classCounts(ops);

  return (
    <Modal show onHide={applying ? undefined : onClose} size="lg" data-testid="sync-review" backdrop="static">
      <Modal.Header closeButton={!applying}>
        <Modal.Title>{title}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {receipt ? (
          <ReceiptView receipt={receipt} />
        ) : applying ? (
          <div className="d-flex align-items-center gap-2" data-testid="sync-progress">
            <Spinner size="sm" />
            <T text="Applying: a safety backup first if anything loses data, then each step in order, then a check that this instance now matches." />
          </div>
        ) : !planned ? (
          !error && <Spinner size="sm" />
        ) : (
          <>
            {ops.length === 0 && (
              <p className="text-secondary">
                <T text="Nothing would change." />
              </p>
            )}
            {groupOps(ops).map((group) => (
              <div key={group.cls} className="mb-3" data-testid={`review-${group.cls}`}>
                <div
                  className={`small fw-bold text-uppercase ${
                    group.cls === "data_destructive" || group.cls === "unsupported" || group.cls === "metadata_delete"
                      ? "text-danger"
                      : "text-secondary"
                  }`}
                >
                  {classLabel(group.cls, t)} ({counts[group.cls] ?? 0})
                </div>
                {group.ops.map((op) => (
                  <Form.Check
                    key={op.id}
                    id={`review-op-${op.id}`}
                    type="checkbox"
                    label={op.summary}
                    checked={ticked.has(op.id)}
                    disabled={!reviewTickable(op, planned.policy)}
                    onChange={() => toggle(op.id)}
                    data-class={op.class}
                    data-op={op.id}
                  />
                ))}
              </div>
            ))}
            {planned.policy?.destructive === "never" && counts.data_destructive > 0 && (
              <p className="small text-secondary">
                <T text="This instance's policy keeps what loses data from being ticked here; only feldspar sync deploy --allow-destructive on the server can apply it." />
              </p>
            )}
            {ops.some((op) => op.class === "data_destructive" && !ticked.has(op.id)) && (
              <p className="small text-secondary">
                <T text="What is left unticked is deferred, not forgotten: it stays here, and the next deploy offers it again." />
              </p>
            )}
            {planned.plan.blockers.length > 0 && (
              <Alert variant="danger" data-testid="review-blockers">
                <div className="fw-bold">
                  <T text="This cannot be applied:" />
                </div>
                <ul className="mb-0">
                  {planned.plan.blockers.map((b) => (
                    <li key={b.message}>{b.message}</li>
                  ))}
                </ul>
              </Alert>
            )}
            {planned.plan.needs_secrets.length > 0 && (
              <p className="small text-secondary">
                {t("Created without a secret this instance has not got: {list}", {
                  list: planned.plan.needs_secrets.join(", "),
                })}
              </p>
            )}
            {planned.plan.needs_credentials.length > 0 && (
              <p className="small text-secondary">
                {t("Git stores this instance cannot clone without credentials: {list}", {
                  list: planned.plan.needs_credentials.join(", "),
                })}
              </p>
            )}
            {planned.policy?.full_backup === "optional" && (
              <Form.Check
                id="review-full-backup"
                type="checkbox"
                label={t("Take a full backup first")}
                checked={fullBackup}
                onChange={(e) => setFullBackup(e.target.checked)}
              />
            )}
            {planned.policy?.full_backup === "required" && (
              <p className="small text-secondary">
                <T text="A full backup is taken first, as this instance's policy requires." />
              </p>
            )}
            {asked !== null && (
              <Form.Group className="mt-2">
                <Form.Label>
                  {t("This loses data, and a safety backup is written first. Type {word} to confirm.", { word: asked })}
                </Form.Label>
                <Form.Control
                  value={typed}
                  onChange={(e) => setTyped(e.target.value)}
                  aria-label={t("Confirmation")}
                  data-testid="review-confirmation"
                />
              </Form.Group>
            )}
          </>
        )}
      </Modal.Body>
      <Modal.Footer>
        {receipt ? (
          <Button onClick={onClose}>
            <T text="Close" />
          </Button>
        ) : (
          <>
            <Button variant="link" onClick={onClose} disabled={applying}>
              <T text="Cancel" />
            </Button>
            <Button
              onClick={() => void confirm()}
              disabled={applying || !planned || !reviewCanConfirm(planned.plan, ticked, typed, word)}
              variant={asked !== null ? "danger" : "primary"}
              data-testid="review-confirm"
            >
              {confirmLabel}
            </Button>
          </>
        )}
      </Modal.Footer>
    </Modal>
  );
}

/** Upload a structure zip and deploy it (§7.4), through the same review as a
 * deploy of the branch tip. */
export function ZipTab({ linked, onDeployed }: { linked: boolean; onDeployed: () => Promise<void> }) {
  const { t } = useT();
  const [file, setFile] = useState<File | null>(null);
  const [uploading, setUploading] = useState(false);
  const [uploaded, setUploaded] = useState<(UploadedStructureZip & { name: string }) | null>(null);
  const [reviewing, setReviewing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const upload = async () => {
    if (!file) return;
    setUploading(true);
    setError(null);
    try {
      const done = await uploadStructureZip(file);
      setUploaded({ ...done, name: file.name });
      setReviewing(true);
    } catch (e) {
      setError(errorMessage(e, t("The file could not be uploaded.")));
    } finally {
      setUploading(false);
    }
  };

  return (
    <Card data-testid="sync-zip">
      <Card.Body>
        <Card.Title>
          <T text="Deploy a structure zip" />
        </Card.Title>
        <p className="text-secondary small">
          {linked ? (
            <T text="The zip takes the branch tip's place: it is planned and applied as a deploy is, what it does not have is deleted (what loses data stays unticked), and it is refused while this instance has uncommitted changes." />
          ) : (
            <T text="This installation is not linked, so the zip is applied additively: what it has is created or updated, and nothing is deleted." />
          )}{" "}
          <T text="A structure zip is written by feldspar backup --structure-only." />
        </p>
        {error && <Alert variant="danger">{error}</Alert>}
        <div className="d-flex gap-2 align-items-center flex-wrap">
          <Form.Control
            type="file"
            accept=".zip,application/zip"
            style={{ maxWidth: "24rem" }}
            aria-label={t("Structure zip")}
            onChange={(e) => {
              const input = e.target as HTMLInputElement;
              setFile(input.files?.[0] ?? null);
              setUploaded(null);
            }}
            data-testid="sync-zip-file"
          />
          <Button disabled={!file || uploading} onClick={() => void upload()} data-testid="sync-zip-upload">
            {uploading ? t("Uploading…") : t("Upload and review")}
          </Button>
          {uploaded && !reviewing && (
            <Button variant="outline-secondary" onClick={() => setReviewing(true)}>
              <T text="Review again" />
            </Button>
          )}
        </div>
      </Card.Body>
      {reviewing && uploaded && (
        <Review
          title={t("Deploy {file} ({count} files)", { file: uploaded.name, count: uploaded.files })}
          word={t("deploy")}
          plan={async () => {
            const plan = await api.planSyncZipDeploy({ upload: uploaded.id });
            return { plan, policy: plan.policy, commit: null };
          }}
          apply={(plan, ticked, fullBackup) =>
            api.syncZipDeploy({
              upload: uploaded.id,
              plan_digest: plan.digest,
              ticked: [...ticked],
              full_backup: fullBackup,
            })
          }
          confirmLabel={t("Deploy")}
          onClose={() => {
            setReviewing(false);
            void onDeployed();
          }}
        />
      )}
    </Card>
  );
}

/** What a deploy or a discard did. */
export function ReceiptView({ receipt }: { receipt: Receipt }) {
  const { t } = useT();
  const list = (title: string, items: readonly string[]) =>
    items.length > 0 && (
      <div className="mb-2">
        <div className="small fw-bold">{title}</div>
        <ul className="small mb-0">
          {items.map((i) => (
            <li key={i}>{i}</li>
          ))}
        </ul>
      </div>
    );
  return (
    <div data-testid="sync-receipt" data-result={receipt.result}>
      <Alert variant={receiptTone(receipt)}>
        <div className="fw-bold">
          {receipt.result === "converged"
            ? t("Applied, and this instance now matches.")
            : receipt.result === "drifted"
              ? t("Applied, but this instance still differs afterwards.")
              : t("A step failed; what came before it was applied.")}
        </div>
        {receipt.failed && (
          <div>
            {receipt.failed.summary}: {receipt.failed.message}
          </div>
        )}
      </Alert>
      {list(t("Applied"), receipt.applied)}
      {list(t("Deferred, as not ticked"), receipt.deferred)}
      {list(t("Not reached"), receipt.not_reached)}
      {list(t("Still different afterwards"), receipt.drift)}
      {list(t("Backups written first"), receipt.backups)}
      {list(t("Created without a secret, to be given here"), [
        ...receipt.needs_secrets,
        ...receipt.needs_secret.map((n) => n.message),
      ])}
      {list(t("Notes"), receipt.notes)}
      <div className="small text-secondary">{t("Ledger entry {id}", { id: receipt.id })}</div>
    </div>
  );
}

// --- History -------------------------------------------------------------------------

export function HistoryTab({ onRestore }: { onRestore: (path: string, value: unknown) => void }) {
  const { t } = useT();
  const [rows, setRows] = useState<Deployment[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .listDeployments({ limit: 100 })
      .then((r) => setRows(r.deployments))
      .catch((e: unknown) => setError(errorMessage(e, t("Could not load the history."))));
  }, [t]);
  if (error) return <Alert variant="danger">{error}</Alert>;
  if (!rows) return <Spinner size="sm" />;
  if (rows.length === 0)
    return (
      <Card>
        <Card.Body className="text-secondary">
          <T text="Nothing has been deployed, committed, discarded or saved through the registry yet." />
        </Card.Body>
      </Card>
    );
  return (
    <Card>
      <Table responsive className="card-table mb-0" data-testid="sync-history">
        <thead>
          <tr>
            <th>
              <T text="When" />
            </th>
            <th>
              <T text="What" />
            </th>
            <th>
              <T text="Result" />
            </th>
            <th>
              <T text="Who" />
            </th>
            <th>
              <T text="Commit" />
            </th>
            <th>
              <T text="Summary" />
            </th>
            <th />
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const restore = restorable(row);
            return (
              <tr key={row.id}>
                <td className="small text-nowrap">{new Date(row.started_at).toLocaleString()}</td>
                <td>{ledgerKindLabel(row.kind, t)}</td>
                <td>
                  <StatusBadge tone={row.result === "converged" ? "green" : row.result === "drifted" ? "yellow" : "red"}>
                    {row.result}
                  </StatusBadge>
                </td>
                <td className="small">
                  {row.actor ?? "—"} <span className="text-secondary">({row.source})</span>
                </td>
                <td className="small">
                  <code>{shortCommit(row.commit)}</code> {row.branch && <span className="text-secondary">{row.branch}</span>}
                </td>
                <td className="small">
                  {ledgerSummary(row, t)}
                  {row.backups.length > 0 && (
                    <div className="text-secondary">{t("Backup: {file}", { file: row.backups.join(", ") })}</div>
                  )}
                </td>
                <td>
                  {restore && (
                    <Button size="sm" variant="outline-secondary" onClick={() => onRestore(restore.path, restore.value)}>
                      <T text="Restore this version" />
                    </Button>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </Table>
    </Card>
  );
}

// --- Link setup ---------------------------------------------------------------------------

export function LinkTab({ onLinked }: { onLinked: () => Promise<void> }) {
  const { t } = useT();
  const [info, setInfo] = useState<SyncLinkInfo | null>(null);
  const [form, setForm] = useState<LinkForm>(() => linkForm(null));
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const got = await api.getSyncLink();
      setInfo(got);
      setForm(linkForm(got));
    } catch (e) {
      setError(errorMessage(e, t("Could not load the link.")));
    }
  }, [t]);
  useEffect(() => void load(), [load]);

  const set = (patch: Partial<LinkForm>) => setForm((f) => ({ ...f, ...patch }));
  const run = async (what: string, f: () => Promise<string>) => {
    setBusy(what);
    setError(null);
    setNotice(null);
    try {
      setNotice(await f());
    } catch (e) {
      setError(errorMessage(e, t("That did not work.")));
    } finally {
      setBusy(null);
    }
  };
  const problem = linkProblem(form, info, t);
  const linked = Boolean(info?.link);
  const publicKey = info?.credential.kind === "deploy_key" ? info.credential.public_key : null;

  return (
    <Card>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {notice && <Alert variant="success">{notice}</Alert>}
        <p className="text-secondary small">
          <T text="Link this installation's structure to a branch of a git repository. Commit pushes what changed here; Deploy applies what is on the branch. Data never travels, only the structure." />
        </p>
        <Row className="g-3">
          <Col md={8}>
            <Form.Group controlId="sync-url">
              <Form.Label>
                <T text="Repository URL" />
              </Form.Label>
              <Form.Control
                value={form.url}
                onChange={(e) => set({ url: e.target.value })}
                placeholder="git@github.com:acme/structure.git"
              />
            </Form.Group>
          </Col>
          <Col md={4}>
            <Form.Group controlId="sync-branch">
              <Form.Label>
                <T text="Branch" />
              </Form.Label>
              <Form.Control value={form.branch} onChange={(e) => set({ branch: e.target.value })} />
            </Form.Group>
          </Col>
          <Col md={12}>
            <Form.Group controlId="sync-subdirectory">
              <Form.Label>
                <T text="Subdirectory" />
              </Form.Label>
              <Form.Control
                value={form.subdirectory}
                onChange={(e) => set({ subdirectory: e.target.value })}
                placeholder={t("(the top of the repository)")}
              />
            </Form.Group>
          </Col>
          <Col md={4}>
            <Form.Group controlId="sync-credential">
              <Form.Label>
                <T text="Credentials" />
              </Form.Label>
              <Form.Select value={form.credential} onChange={(e) => set({ credential: e.target.value })}>
                <option value="none">{t("None (a public or local repository)")}</option>
                <option value="deploy_key">{t("A deploy key generated here (SSH)")}</option>
                <option value="https_token">{t("An HTTPS token")}</option>
              </Form.Select>
            </Form.Group>
          </Col>
          {form.credential === "deploy_key" && (
            <Col md={8}>
              <Form.Label>
                <T text="Public key" />
              </Form.Label>
              {publicKey ? (
                <pre className="small border rounded p-2 text-wrap text-break mb-1" data-testid="sync-public-key">
                  {publicKey}
                </pre>
              ) : (
                <div className="small text-secondary mb-1">
                  <T text="No key yet." />
                </div>
              )}
              <Button
                size="sm"
                variant="outline-secondary"
                disabled={busy !== null}
                onClick={() =>
                  void run("key", async () => {
                    await api.generateSyncKey();
                    await load();
                    return t("A new key was generated. Add its public key to the repository as a deploy key, with write access if this instance commits.");
                  })
                }
              >
                {publicKey ? t("Generate a new key") : t("Generate a key")}
              </Button>
            </Col>
          )}
          {form.credential === "https_token" && (
            <>
              <Col md={4}>
                <Form.Group controlId="sync-username">
                  <Form.Label>
                    <T text="Username" />
                  </Form.Label>
                  <Form.Control value={form.username} onChange={(e) => set({ username: e.target.value })} />
                </Form.Group>
              </Col>
              <Col md={4}>
                <Form.Group controlId="sync-token">
                  <Form.Label>
                    <T text="Token" />
                  </Form.Label>
                  <Form.Control
                    type="password"
                    value={form.token}
                    onChange={(e) => set({ token: e.target.value })}
                    placeholder={info?.credential.kind === "https_token" ? t("(stored; type to replace)") : ""}
                    autoComplete="new-password"
                  />
                </Form.Group>
              </Col>
            </>
          )}
          <Col md={6}>
            <Form.Group controlId="sync-destructive">
              <Form.Label>
                <T text="Changes that lose data" />
              </Form.Label>
              <Form.Select value={form.destructive} onChange={(e) => set({ destructive: e.target.value })}>
                <option value="confirm">{t("Ticked one by one, with a typed confirmation")}</option>
                <option value="never">{t("Never from here; only on the server with --allow-destructive")}</option>
              </Form.Select>
            </Form.Group>
          </Col>
          <Col md={6}>
            <Form.Group controlId="sync-full-backup">
              <Form.Label>
                <T text="Full backup before each deploy" />
              </Form.Label>
              <Form.Select value={form.fullBackup} onChange={(e) => set({ fullBackup: e.target.value })}>
                <option value="off">{t("Off")}</option>
                <option value="optional">{t("Offered on the review")}</option>
                <option value="required">{t("Always")}</option>
              </Form.Select>
            </Form.Group>
          </Col>
        </Row>
        {problem && <div className="small text-secondary mt-3">{problem}</div>}
        <div className="d-flex gap-2 mt-3">
          <Button
            variant="outline-secondary"
            disabled={busy !== null || problem !== null}
            onClick={() =>
              void run("test", async () => {
                const { branches } = await api.testSyncLink(linkBody(form));
                return branches.length === 0
                  ? t("The repository answered; it has no branches yet.")
                  : t("The repository answered; its branches: {list}", { list: branches.join(", ") });
              })
            }
          >
            <IconRefresh /> {busy === "test" ? t("Testing…") : t("Test the connection")}
          </Button>
          <Button
            disabled={busy !== null || problem !== null}
            onClick={() =>
              void run("save", async () => {
                const done = await api.setSyncLink(linkBody(form));
                await load();
                await onLinked();
                return done.base_taken
                  ? t("Linked, at {tip}. Status now compares this instance with the branch.", { tip: shortCommit(done.tip) })
                  : t("The link is saved.");
              })
            }
            data-testid="sync-link-save"
          >
            {linked ? t("Save the link") : t("Link")}
          </Button>
          {linked && (
            <Button
              variant="outline-danger"
              className="ms-auto"
              disabled={busy !== null}
              onClick={() => {
                if (!window.confirm(t("Unlink? The structure here stays as it is; Status and Deploy go."))) return;
                void run("unlink", async () => {
                  await api.deleteSyncLink();
                  await load();
                  await onLinked();
                  return t("Unlinked.");
                });
              }}
            >
              <T text="Unlink" />
            </Button>
          )}
        </div>
      </Card.Body>
    </Card>
  );
}
