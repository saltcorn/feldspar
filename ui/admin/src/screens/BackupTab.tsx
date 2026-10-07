// The Settings screen's Backup tab: take a backup, or restore one.
//
// Two buttons and one dialog, twice. The dialog is the same component both times
// (`IncludeDialog`) because the two questions are the same question asked of two
// sources: "of everything here, what should this cover?" — where *here* is this
// installation when backing up, and an uploaded file when restoring. The server
// describes both in one shape, so this screen never asks which it is looking at.
//
// The arithmetic of the selection — that rows cannot be included without their
// table, what "everything" means, what the summary line says — is in `backup.ts`,
// where it is tested without a browser. What is left here is the flow: the
// buttons, the busy states, and what the admin is told afterwards.

import { useCallback, useEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, createBackup, errorMessage, uploadBackup, type UploadedBackup } from "../api";
import {
  NO_CONTENTS,
  choices,
  dataChoices,
  hasAnalytics,
  isEmpty,
  summarise,
  withAnalytics,
  withTableData,
  withTables,
  type BackupContents,
  type BackupSelection,
} from "../backup";
import { IconDownload, IconUpload } from "../icons";
import { AlertBody } from "../layout";
import { MultiSelect } from "../multiSelect";
import type { RestoreBackupResponse } from "../client";
import { T, useT } from "../i18n";

/** What the restore flow is doing: nothing, holding an uploaded file's contents,
 * or showing what a finished restore did. */
type Restore =
  | { stage: "idle" }
  | { stage: "choosing"; uploaded: UploadedBackup; selection: BackupSelection }
  | { stage: "done"; report: RestoreBackupResponse };

export function BackupTab() {
  const { t } = useT();
  const [contents, setContents] = useState<BackupContents>(NO_CONTENTS);
  const [selection, setSelection] = useState<BackupSelection | null>(null);
  const [choosing, setChoosing] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [taken, setTaken] = useState(false);
  const [restore, setRestore] = useState<Restore>({ stage: "idle" });
  const fileInput = useRef<HTMLInputElement>(null);

  /** What this server has, and the selection the admin last backed up with. */
  const load = useCallback(async () => {
    try {
      const options = await api.getBackupOptions();
      setContents(options.available);
      setSelection(options.include);
    } catch (e) {
      setError(errorMessage(e, "Could not read what there is to back up."));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const takeBackup = async (include: BackupSelection) => {
    setBusy("Building the backup…");
    setError(null);
    setTaken(false);
    try {
      await createBackup(include);
      setChoosing(false);
      setTaken(true);
      // The server remembers the selection as part of taking the backup, so the
      // options are read back: the next dialog opens on what was just used.
      await load();
    } catch (e) {
      setError(errorMessage(e, "Could not build the backup."));
    } finally {
      setBusy(null);
    }
  };

  const readFile = async (file: File | undefined) => {
    if (!file) return;
    setBusy("Reading the backup…");
    setError(null);
    setRestore({ stage: "idle" });
    try {
      const uploaded = await uploadBackup(file);
      setRestore({ stage: "choosing", uploaded, selection: uploaded.include });
    } catch (e) {
      setError(errorMessage(e, "That file could not be read as a backup."));
    } finally {
      setBusy(null);
    }
  };

  const runRestore = async (uploaded: UploadedBackup, include: BackupSelection) => {
    setBusy("Restoring…");
    setError(null);
    try {
      const report = await api.restoreBackup({ id: uploaded.id, include });
      setRestore({ stage: "done", report });
      // A restore can add tables, stores and applications: what there is to back
      // up has changed.
      await load();
    } catch (e) {
      setError(errorMessage(e, "The restore failed."));
      setRestore({ stage: "idle" });
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      {error && (
        <Alert variant="danger" dismissible onClose={() => setError(null)}>
          <AlertBody>{error}</AlertBody>
        </Alert>
      )}
      {taken && (
        <Alert variant="success" dismissible onClose={() => setTaken(false)}>
          <AlertBody>
            <T text="The backup has been downloaded." />
          </AlertBody>
        </Alert>
      )}

      <div className="card mb-4">
        <div className="card-header">
          <div>
            <h3 className="card-title"><T text="Backup" /></h3>            
          </div>
        </div>
        <div className="card-body">
          {selection === null ? (
            <Spinner animation="border" role="status" size="sm" />
          ) : (
            <>
              <p className="text-secondary mb-3">
                {t("Currently included: {summary}", {
                  summary: summarise(selection, contents),
                })}
              </p>
              <div className="btn-list">
                <Button onClick={() => setChoosing(true)} disabled={busy !== null}>
                  <IconDownload /> <T text="Backup now" />
                </Button>
              </div>
              {/* Said where the choice is made, not in a footnote: a backup carries
                  password hashes, a file store's credentials and the TLS private
                  key, so the file is exactly as sensitive as the database. */}
              <p className="form-hint mt-3 mb-0 text-secondary">
                <T text="A backup contains everything needed to restore this installation, including password hashes, file-store credentials and the SSL private key." />
              </p>
            </>
          )}
        </div>
      </div>

      <div className="card">
        <div className="card-header">
          <div>
            <h3 className="card-title"><T text="Restore" /></h3>
            <p className="card-subtitle text-secondary mb-0">
              <T text="Read a backup file and put back the parts of it you choose. A Saltcorn 1 backup works too: its tables, rows, users, files and actions are imported, and the restore says what it could not bring across. Nothing already on this server is deleted or overwritten: tables, users and file stores that are already here are left as they are, and the restore says what it skipped. Restored applications are built and start serving straight away, so a restore that includes one takes as long as its build does." />
            </p>
          </div>
        </div>
        <div className="card-body">
          <div className="btn-list">
            <Button
              variant="outline-primary"
              disabled={busy !== null}
              onClick={() => fileInput.current?.click()}
            >
              <IconUpload /> <T text="Restore" />
            </Button>
          </div>
          <input
            ref={fileInput}
            type="file"
            accept=".zip,application/zip"
            className="d-none"
            onChange={(e) => {
              void readFile(e.target.files?.[0]);
              // Cleared so choosing the same file twice fires a change both times.
              e.target.value = "";
            }}
          />

          {restore.stage === "done" && (
            <div className="mt-3">
              <RestoreReport report={restore.report} />
            </div>
          )}
        </div>
      </div>

      {busy !== null && (
        <div className="mt-3 text-secondary" role="status">
          <Spinner animation="border" size="sm" className="me-2" />
          {busy}
        </div>
      )}

      {selection !== null && (
        <IncludeDialog
          show={choosing}
          title={t("What should the backup include?")}
          confirm="Backup now"
          busy={busy}
          contents={contents}
          selection={selection}
          onChange={setSelection}
          onCancel={() => setChoosing(false)}
          onConfirm={() => void takeBackup(selection)}
        />
      )}

      {restore.stage === "choosing" && (
        <IncludeDialog
          show
          title={t("What should be restored?")}
          subtitle={restoreSubtitle(restore.uploaded)}
          confirm="Restore"
          busy={busy}
          contents={restore.uploaded.available}
          selection={restore.selection}
          onChange={(selection) => setRestore({ ...restore, selection })}
          onCancel={() => setRestore({ stage: "idle" })}
          onConfirm={() => void runRestore(restore.uploaded, restore.selection)}
        />
      )}
    </>
  );
}

/** What the restore dialog says above the tick boxes: when the backup was taken
 * and what wrote it.
 *
 * The source is worth a line of its own because one answer to it is "Saltcorn
 * 1.7.0, imported" — a file this server translated, which carries tables, rows,
 * files, users and actions and leaves v1's views and pages behind. An admin
 * should see that before they press Restore, not afterwards in the report. */
function restoreSubtitle(uploaded: UploadedBackup): string | undefined {
  const parts: string[] = [];
  if (uploaded.created_at)
    parts.push(`Backup taken ${new Date(uploaded.created_at).toLocaleString()}.`);
  if (uploaded.source) parts.push(`From ${uploaded.source}.`);
  return parts.length > 0 ? parts.join(" ") : undefined;
}

/** The tick boxes and pickers, over whatever is on offer.
 *
 * Every row is conditional on the offering holding something of that kind, which
 * is what lets one dialog serve both flows: a backup with no applications in it
 * simply has no applications row to untick. */
function IncludeDialog({
  show,
  title,
  subtitle,
  confirm,
  busy,
  contents,
  selection,
  onChange,
  onCancel,
  onConfirm,
}: {
  show: boolean;
  title: string;
  subtitle?: string;
  confirm: string;
  busy: string | null;
  contents: BackupContents;
  selection: BackupSelection;
  onChange: (selection: BackupSelection) => void;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const { t } = useT();
  const nothing = isEmpty(selection);
  return (
    <Modal show={show} onHide={onCancel} size="lg" scrollable>
      <Modal.Header closeButton>
        <Modal.Title className="h4">{title}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {subtitle && <p className="text-secondary">{subtitle}</p>}

        {contents.tables.length > 0 && (
          <>
            <Form.Group className="mb-3" controlId="backup-tables">
              <Form.Label><T text="Table definitions" /></Form.Label>
              <MultiSelect
                id="backup-tables"
                options={choices(contents.tables, "row")}
                selected={selection.tables}
                onChange={(tables) => onChange(withTables(selection, tables))}
                placeholder={t("No tables")}
              />
              <Form.Text muted>
                <T text="A table's columns, its access rules and its ownership formula." />
              </Form.Text>
            </Form.Group>

            <Form.Group className="mb-3" controlId="backup-table-data">
              <Form.Label><T text="Table data" /></Form.Label>
              <MultiSelect
                id="backup-table-data"
                options={dataChoices(contents, selection)}
                selected={selection.table_data}
                onChange={(data) => onChange(withTableData(selection, data))}
                placeholder={t("No rows")}
                emptyText="Choose a table above first."
              />
              {/* The rule, where it applies: the picker above is the list this one
                  offers, so unticking a table takes its rows with it. */}
              <Form.Text muted>
                <T text="The rows themselves. Only a table whose definition is included can have its rows included." />
              </Form.Text>
            </Form.Group>
          </>
        )}

        {contents.applications.length > 0 && (
          <Form.Group className="mb-3" controlId="backup-applications">
            <Form.Label><T text="Applications" /></Form.Label>
            <MultiSelect
              id="backup-applications"
              options={choices(contents.applications, "")}
              selected={selection.applications}
              onChange={(applications) => onChange({ ...selection, applications })}
              placeholder={t("No applications")}
            />
            <Form.Text muted>
              <T text="An application's definition — its framework, its API and the tables it exposes. The built bundle is not in the backup: a restored application is rebuilt from the source its file store carries, which is what makes it serve again without anybody pressing Build." />
            </Form.Text>
          </Form.Group>
        )}

        {contents.file_stores.length > 0 && (
          <Form.Group className="mb-3" controlId="backup-file-stores">
            <Form.Label><T text="Files" /></Form.Label>
            <MultiSelect
              id="backup-file-stores"
              options={choices(contents.file_stores, "file")}
              selected={selection.file_stores}
              onChange={(file_stores) => onChange({ ...selection, file_stores })}
              placeholder={t("No file stores")}
            />
            <Form.Text muted>
              <T text="Each store's definition, every file in it, and each file's access rules." />
            </Form.Text>
          </Form.Group>
        )}

        <div className="mb-2">
          {contents.users > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-users"
              className="mb-2"
              checked={selection.users}
              onChange={(e) => onChange({ ...selection, users: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Users and roles" /></span>
                  <div className="text-muted small">
                    {/* One message with a plural form, not a ternary over two
                      English words: which forms a language needs is CLDR's
                      answer and not this file's. */}
                    {t(
                      "{count} accounts, with their password hashes and the roles they hold.",
                      { count: contents.users },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.modules > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-modules"
              className="mb-2"
              checked={selection.modules}
              onChange={(e) => onChange({ ...selection, modules: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Modules" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} installed modules, with their settings and permissions. A restore reinstalls each one from where it came, so the server needs npm or pip and whatever the module was installed from.",
                      { count: contents.modules },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.db_connections > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-db-connections"
              className="mb-2"
              checked={selection.db_connections}
              onChange={(e) => onChange({ ...selection, db_connections: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Database connections" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} connections to other databases, with their passwords. Their tables stay in those databases and are not copied into the backup.",
                      { count: contents.db_connections },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.streams > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-streams"
              className="mb-2"
              checked={selection.streams}
              onChange={(e) => onChange({ ...selection, streams: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Streams" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} streams, with their connection settings and secrets. What they have delivered is not included.",
                      { count: contents.streams },
                    )}
                  </div>
                </>
              }
            />
          )}
          {hasAnalytics(contents) && (
            <Form.Check
              type="checkbox"
              id="backup-analytics"
              className="mb-2"
              checked={selection.analytics}
              onChange={(e) => onChange(withAnalytics(selection, e.target.checked))}
              label={
                <>
                  <span className="fw-semibold"><T text="Analytics" /></span>
                  <div className="text-muted small">
                    {t("Datasets: {datasets}. Models: {models}. Workspaces: {workspaces}.", {
                      datasets: contents.datasets,
                      models: contents.models,
                      workspaces: contents.workspaces,
                    })}
                  </div>
                </>
              }
            />
          )}
          {contents.fits > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-fits"
              className="mb-2"
              checked={selection.fits}
              disabled={!selection.analytics}
              onChange={(e) => onChange({ ...selection, fits: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Model fits" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} fitted model instances, with their output frames and posterior draws. These can be large; without them a restored model has to be fitted again.",
                      { count: contents.fits },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.llm_providers > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-llm-providers"
              className="mb-2"
              checked={selection.llm_providers}
              onChange={(e) => onChange({ ...selection, llm_providers: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="LLM providers" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} LLM providers, with their models and API keys. An agent is only restored if the provider it uses is.",
                      { count: contents.llm_providers },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.agents > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-agents"
              className="mb-2"
              checked={selection.agents}
              onChange={(e) => onChange({ ...selection, agents: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Agents" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} agents, with their prompts and enabled traits. Their runs are not included.",
                      { count: contents.agents },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.triggers > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-triggers"
              className="mb-2"
              checked={selection.triggers}
              onChange={(e) => onChange({ ...selection, triggers: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Triggers" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} triggers. A trigger that fires on a table whose definition is not included is left out with it.",
                      { count: contents.triggers },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.views > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-views"
              className="mb-2"
              checked={selection.views}
              disabled={selection.applications.length === 0}
              onChange={(e) => onChange({ ...selection, views: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Views" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} Saltcorn UI views, in the applications chosen above. Restored, they replace the views the application has.",
                      { count: contents.views },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.pages > 0 && (
            <Form.Check
              type="checkbox"
              id="backup-pages"
              className="mb-2"
              checked={selection.pages}
              disabled={selection.applications.length === 0}
              onChange={(e) => onChange({ ...selection, pages: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Pages" /></span>
                  <div className="text-muted small">
                    {t(
                      "{count} Saltcorn UI pages, in the applications chosen above. Restored, they replace the pages the application has.",
                      { count: contents.pages },
                    )}
                  </div>
                </>
              }
            />
          )}
          {contents.ssl && (
            <Form.Check
              type="checkbox"
              id="backup-ssl"
              className="mb-2"
              checked={selection.ssl}
              onChange={(e) => onChange({ ...selection, ssl: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="SSL settings" /></span>
                  <div className="text-muted small">
                    <T text="The certificate source and, in" /> <code>custom</code> <T text="mode, the certificate and its private key." />
                  </div>
                </>
              }
            />
          )}
          {contents.settings && (
            <Form.Check
              type="checkbox"
              id="backup-settings"
              className="mb-2"
              checked={selection.settings}
              onChange={(e) => onChange({ ...selection, settings: e.target.checked })}
              label={
                <>
                  <span className="fw-semibold"><T text="Other settings" /></span>
                  <div className="text-muted small">
                    <T text="Email, localisation and development settings, including the SMTP password." />
                  </div>
                </>
              }
            />
          )}
        </div>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" type="button" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button type="button" disabled={busy !== null || nothing} onClick={onConfirm}>
          {busy !== null ? "Working…" : confirm}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/** What a finished restore did and did not do.
 *
 * Both lists, always: a restore that skipped an account already on the server has
 * not failed, and reporting only the successes would hide the one thing the admin
 * needs to know. */
function RestoreReport({ report }: { report: RestoreBackupResponse }) {
  return (
    <>
      <Alert variant={report.warnings.length > 0 ? "warning" : "success"}>
        <AlertBody>
          <strong>
            {report.restored.length === 0
              ? "Nothing was restored."
              : `Restored ${report.restored.length} ${
                  report.restored.length === 1 ? "thing" : "things"
                }.`}
          </strong>
          {report.warnings.length > 0 && (
            <>
              <div className="mt-2">Skipped:</div>
              <ul className="mb-0">
                {report.warnings.map((warning, i) => (
                  <li key={i}>{warning}</li>
                ))}
              </ul>
            </>
          )}
        </AlertBody>
      </Alert>
      {report.restored.length > 0 && (
        <details>
          <summary className="text-secondary"><T text="What was restored" /></summary>
          <ul className="mt-2 text-secondary">
            {report.restored.map((line, i) => (
              <li key={i}>{line}</li>
            ))}
          </ul>
        </details>
      )}
    </>
  );
}
