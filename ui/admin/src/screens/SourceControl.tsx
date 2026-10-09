// A store's working copy, drawn like VS Code's Source Control view: a commit
// message with its button beside it (there is the width for it here, where VS
// Code puts it underneath), the changed files in Staged Changes and Changes with
// VS Code's letters and hover actions, and a branch selector underneath.
//
// There is no editor on this screen, so VS Code's Open File / Open Changes
// actions have no counterpart — a row stages, unstages or discards, and that is
// all. The model (rows, letters, which button is showing) is `sourceControl.ts`.

import { useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import {
  IconArrowBackUp,
  IconArrowDown,
  IconArrowUp,
  IconCheck,
  IconGitBranch,
  IconMinus,
  IconPlus,
} from "../icons";
import { T, useT } from "../i18n";
import {
  NEW_BRANCH,
  branchNameProblem,
  changeRows,
  commitInput,
  discardConfirmation,
  parseScmStatus,
  primaryAction,
  rowsIn,
  splitPath,
  type ChangeRow,
  type ScmOperation,
  type ScmStatus,
} from "../sourceControl";

/** Operations whose output is worth showing on success: the ones that talk to a
 * remote, where git's own words ("Everything up-to-date", "rejected") are the
 * answer. A stage or a commit shows its result in the list instead. */
const SHOW_OUTPUT: readonly ScmOperation[] = ["clone", "pull", "push"];

export function SourceControl({
  storeId,
  status,
  report,
  declared,
  onStatus,
}: {
  storeId: string;
  status: ScmStatus;
  /** The status operation's prose, shown when there is no working copy to draw. */
  report: string;
  /** The operations the backend declares; a control whose operation is missing is not drawn. */
  declared: readonly string[];
  onStatus: (status: ScmStatus) => void;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState<ScmOperation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [message, setMessage] = useState("");

  const has = (op: ScmOperation) => declared.includes(op);

  const run = async (op: ScmOperation, input: Record<string, unknown> = {}): Promise<boolean> => {
    setBusy(op);
    setError(null);
    setOutput(null);
    let ok = false;
    try {
      const res = await api.runFileStoreOperation(storeId, op, { input });
      const next = parseScmStatus(res.data);
      if (next) onStatus(next);
      if (SHOW_OUTPUT.includes(op) && res.output.trim() !== "") setOutput(res.output);
      ok = true;
    } catch (err) {
      // git's own message — "rejected: non-fast-forward", "would be overwritten
      // by checkout" — which is the actionable part.
      setError(errorMessage(err, `Could not run ${op}.`));
    }
    setBusy(null);
    return ok;
  };

  const rows = changeRows(status, t);
  const primary = primaryAction(status, message, t);
  const disabled = busy !== null;

  const pressPrimary = async () => {
    if (primary.blocked || disabled) return;
    if (primary.kind === "commit") {
      if (await run("commit", commitInput(status, message))) setMessage("");
    } else if (primary.kind === "pull") {
      await run("pull");
    } else {
      await run("push");
    }
  };

  const discard = async (targets: ChangeRow[]) => {
    if (targets.length === 0 || !window.confirm(discardConfirmation(targets))) return;
    await run("discard", { paths: targets.map((row) => row.path).join("\n") });
  };
  const stage = (paths: string[]) => run("stage", { paths: paths.join("\n") });
  const unstage = (paths: string[]) => run("unstage", { paths: paths.join("\n") });

  return (
    <div className="scm">
      {error && (
        <Alert variant="danger" onClose={() => setError(null)} dismissible>
          <pre className="mb-0 small text-break text-pre-wrap">{error}</pre>
        </Alert>
      )}
      {output && (
        <Alert variant="secondary" onClose={() => setOutput(null)} dismissible>
          <pre className="mb-0 small text-break text-pre-wrap">{output}</pre>
        </Alert>
      )}

      {!status.cloned ? (
        status.canClone && has("clone") ? (
          <div>
            <p className="text-secondary mb-2">
              <T text="The repository has not been cloned yet." />
            </p>
            <Button onClick={() => void run("clone")} disabled={disabled}>
              {busy === "clone" ? t("Cloning…") : t("Clone repository")}
            </Button>
          </div>
        ) : (
          <pre className="small text-break text-pre-wrap mb-0">{report}</pre>
        )
      ) : (
        <>
          {has("commit") && (
            <div className="d-flex gap-2 align-items-start mb-3">
              <CommitMessage
                value={message}
                onChange={setMessage}
                placeholder={
                  status.branch
                    ? t('Message (Ctrl+Enter to commit on "{branch}")', { branch: status.branch })
                    : t("Message (Ctrl+Enter to commit)")
                }
                onSubmit={() => void pressPrimary()}
                disabled={disabled}
              />
              <Button
                className="scm-primary text-nowrap"
                onClick={() => void pressPrimary()}
                disabled={disabled || primary.blocked !== null}
                title={primary.blocked ?? undefined}
              >
                {busy === "commit" || busy === "push" || busy === "pull" ? (
                  <Spinner animation="border" size="sm" className="me-2" />
                ) : primary.kind === "commit" ? (
                  <IconCheck className="icon-2" />
                ) : primary.kind === "pull" ? (
                  <IconArrowDown className="icon-2" />
                ) : (
                  <IconArrowUp className="icon-2" />
                )}
                {primary.kind === "commit"
                  ? t("Commit")
                  : primary.kind === "publish"
                    ? t("Publish branch")
                    : primary.kind === "pull"
                      ? `${t("Pull")} ↓${status.behind}`
                      : `${t("Push")} ↑${status.ahead}`}
              </Button>
            </div>
          )}

          <ChangeGroupList
            title={t("Merge changes")}
            rows={rowsIn(rows, "merge")}
            disabled={disabled}
            actions={has("stage") ? [{ kind: "stage", onRun: (r) => void stage(r.map((x) => x.path)) }] : []}
          />
          <ChangeGroupList
            title={t("Staged changes")}
            rows={rowsIn(rows, "staged")}
            disabled={disabled}
            actions={has("unstage") ? [{ kind: "unstage", onRun: (r) => void unstage(r.map((x) => x.path)) }] : []}
          />
          <ChangeGroupList
            title={t("Changes")}
            rows={rowsIn(rows, "unstaged")}
            disabled={disabled}
            actions={[
              ...(has("discard") ? [{ kind: "discard" as const, onRun: (r: ChangeRow[]) => void discard(r) }] : []),
              ...(has("stage") ? [{ kind: "stage" as const, onRun: (r: ChangeRow[]) => void stage(r.map((x) => x.path)) }] : []),
            ]}
          />
          {rows.length === 0 && (
            <p className="text-secondary small mb-3">
              <T text="No uncommitted changes." />
            </p>
          )}

          {has("checkout") && (
            <BranchSelector
              status={status}
              disabled={disabled}
              pulling={busy === "pull"}
              canPull={has("pull")}
              run={run}
            />
          )}
          {status.lastCommit && (
            <div className="text-secondary small mt-2 text-break">
              <T text="Last commit: {commit}" args={{ commit: status.lastCommit }} />
            </div>
          )}
        </>
      )}
    </div>
  );
}

/** The commit message: one line, growing as lines are added (and as a long one
 * wraps), with Ctrl/⌘+Enter committing — VS Code's own shortcut. */
function CommitMessage({
  value,
  onChange,
  placeholder,
  onSubmit,
  disabled,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  onSubmit: () => void;
  disabled: boolean;
}) {
  const ref = useRef<HTMLTextAreaElement>(null);

  // Set through the CSSOM, which the CSP's `style-src` does not govern: the
  // height is measured, so it has no class to live in.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    const border = el.offsetHeight - el.clientHeight;
    el.style.height = `${el.scrollHeight + border}px`;
  }, [value]);

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      onSubmit();
    }
  };

  return (
    <Form.Control
      as="textarea"
      ref={ref}
      rows={1}
      className="scm-message"
      value={value}
      placeholder={placeholder}
      aria-label={placeholder}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={onKeyDown}
      disabled={disabled}
    />
  );
}

type RowActionKind = "stage" | "unstage" | "discard";

interface RowAction {
  kind: RowActionKind;
  /** Run against the rows it applies to: one row, or the whole group. */
  onRun: (rows: ChangeRow[]) => void;
}

/** One group: a header whose actions apply to every row, then the rows, each
 * with the same actions for itself. Actions appear on hover, as in VS Code. A
 * group with nothing in it is not drawn. */
function ChangeGroupList({
  title,
  rows,
  actions,
  disabled,
}: {
  title: string;
  rows: ChangeRow[];
  actions: RowAction[];
  disabled: boolean;
}) {
  const { t } = useT();
  if (rows.length === 0) return null;

  const label = (kind: RowActionKind, all: boolean) =>
    ({
      stage: all ? t("Stage all changes") : t("Stage changes"),
      unstage: all ? t("Unstage all changes") : t("Unstage changes"),
      discard: all ? t("Discard all changes") : t("Discard changes"),
    })[kind];

  return (
    <div className="scm-group mb-3">
      <div className="scm-row scm-group-header">
        <span className="scm-name fw-bold small text-uppercase">{title}</span>
        <span className="scm-actions">
          {actions.map((a) => (
            <IconButton
              key={a.kind}
              label={label(a.kind, true)}
              onClick={() => a.onRun(rows)}
              disabled={disabled}
            >
              <ActionIcon kind={a.kind} />
            </IconButton>
          ))}
        </span>
        <span className="badge bg-secondary-lt scm-count">{rows.length}</span>
      </div>
      <ul className="list-unstyled mb-0">
        {rows.map((row) => {
          const { name, dir } = splitPath(row.path);
          return (
            <li key={`${row.group}:${row.path}`} className="scm-row" title={`${row.path} • ${row.label}`}>
              <span className={`scm-name ${row.deleted ? "text-decoration-line-through" : ""}`}>
                {name}
                {dir && <span className="text-secondary small ms-2">{dir}</span>}
              </span>
              <span className="scm-actions">
                {actions.map((a) => (
                  <IconButton
                    key={a.kind}
                    label={label(a.kind, false)}
                    onClick={() => a.onRun([row])}
                    disabled={disabled}
                  >
                    <ActionIcon kind={a.kind} />
                  </IconButton>
                ))}
              </span>
              <span className={`scm-letter ${letterClass(row)}`}>{row.letter}</span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function ActionIcon({ kind }: { kind: RowActionKind }) {
  if (kind === "stage") return <IconPlus className="icon-1" />;
  if (kind === "unstage") return <IconMinus className="icon-1" />;
  return <IconArrowBackUp className="icon-1" />;
}

/** The letter's colour, after VS Code's `gitDecoration.*` defaults. */
function letterClass(row: ChangeRow): string {
  if (row.group === "merge") return "text-purple";
  if (row.letter === "U" || row.letter === "A") return "text-green";
  if (row.letter === "D") return "text-red";
  return "text-yellow";
}

/** Switch branch, or make one: the current branch, every other branch, and
 * "Create new branch…", which turns the selector into a name box. */
function BranchSelector({
  status,
  disabled,
  pulling,
  canPull,
  run,
}: {
  status: ScmStatus;
  disabled: boolean;
  pulling: boolean;
  canPull: boolean;
  run: (op: ScmOperation, input: Record<string, unknown>) => Promise<boolean>;
}) {
  const { t } = useT();
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");

  // An empty repository lists no branches, but it is still *on* one.
  const branches =
    status.branch && !status.branches.includes(status.branch)
      ? [status.branch, ...status.branches]
      : status.branches;
  const problem = branchNameProblem(name, branches, t);

  const create = async () => {
    if (problem) return;
    if (await run("checkout", { branch: name.trim(), create: true })) {
      setCreating(false);
      setName("");
    }
  };

  return (
    <div className="d-flex gap-2 align-items-start">
      <IconGitBranch className="icon-2 mt-2 text-secondary" />
      {creating ? (
        <>
          <Form.Group className="flex-grow-1">
            <Form.Control
              autoFocus
              value={name}
              placeholder={t("New branch name")}
              aria-label={t("New branch name")}
              isInvalid={name.trim() !== "" && problem !== null}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void create();
                if (e.key === "Escape") setCreating(false);
              }}
              disabled={disabled}
            />
            <Form.Control.Feedback type="invalid">{problem}</Form.Control.Feedback>
            <Form.Text>
              <T text="Created from {branch}." args={{ branch: status.branch || "HEAD" }} />
            </Form.Text>
          </Form.Group>
          <Button onClick={() => void create()} disabled={disabled || problem !== null}>
            <T text="Create" />
          </Button>
          <Button variant="outline-secondary" onClick={() => setCreating(false)} disabled={disabled}>
            <T text="Cancel" />
          </Button>
        </>
      ) : (
        <Form.Select
          className="scm-branch"
          aria-label={t("Branch")}
          value={status.branch}
          disabled={disabled}
          onChange={(e) => {
            const value = e.target.value;
            if (value === NEW_BRANCH) {
              setName("");
              setCreating(true);
            } else if (value !== status.branch) {
              void run("checkout", { branch: value, create: false });
            }
          }}
        >
          {status.branch === "" && (
            <option value="" disabled>
              {t("(no branch)")}
            </option>
          )}
          {branches.map((b) => (
            <option key={b} value={b}>
              {b}
            </option>
          ))}
          <option value={NEW_BRANCH}>{t("+ Create new branch…")}</option>
        </Form.Select>
      )}
      {!creating && canPull && (
        <Button
          variant="outline-secondary"
          className="text-nowrap"
          onClick={() => void run("pull", {})}
          disabled={disabled}
          title={t("Fetch the remote and merge it into this branch")}
        >
          {pulling ? (
            <Spinner animation="border" size="sm" className="me-2" />
          ) : (
            <IconArrowDown className="icon-2" />
          )}
          {status.behind > 0 ? `${t("Pull")} ↓${status.behind}` : t("Pull")}
        </Button>
      )}
    </div>
  );
}

/** A small borderless icon button, VS Code's toolbar style, labelled for
 * pointer (tooltip) and screen reader alike. */
export function IconButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className="scm-icon-button"
      title={label}
      aria-label={label}
      onClick={onClick}
      disabled={disabled}
    >
      {children}
    </button>
  );
}
