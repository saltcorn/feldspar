// The Settings screen's Backup tab: take a backup, restore one, or have the
// server take them on a schedule, to a directory, an SFTP server or an S3
// bucket (the Automated backups card, at the end).
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

import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

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
import {
  DEFAULT_SFTP_PORT,
  DESTINATION_KINDS,
  FREQUENCIES,
  MAX_RETENTION_DAYS,
  destinationKindLabel,
  editScheduleForm,
  frequencyLabel,
  isStoredSecret,
  newScheduleForm,
  scheduleBody,
  scheduleFormErrors,
  scheduleStatus,
  type BackupSchedule,
  type DestinationKind,
  type Frequency,
  type ScheduleField,
  type ScheduleForm,
} from "../backupSchedules";
import { SECRET_SENTINEL } from "../dbConnection";
import { IconDownload, IconPencil, IconPlus, IconTrash, IconUpload } from "../icons";
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

      {/* Backup and Restore side by side, the automated backups beneath them:
          the two things done now, then the one that happens on its own. */}
      <div className="row row-cards mb-4">
        <div className="col-md-6">
          <div className="card h-100">
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
        </div>
        <div className="col-md-6">
          <div className="card h-100">
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
        </div>
      </div>

      {busy !== null && (
        <div className="mb-4 text-secondary" role="status">
          <Spinner animation="border" size="sm" className="me-2" />
          {busy}
        </div>
      )}

      <AutomatedBackups contents={contents} cardSelection={selection} />

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

/** The backup and restore dialog: a title, the tick boxes and pickers, and a
 * button. */
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
  const nothing = isEmpty(selection);
  return (
    <Modal show={show} onHide={onCancel} size="lg" scrollable>
      <Modal.Header closeButton>
        <Modal.Title className="h4">{title}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {subtitle && <p className="text-secondary">{subtitle}</p>}

        <IncludeFields
          idPrefix="backup"
          contents={contents}
          selection={selection}
          onChange={onChange}
        />
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

/** The tick boxes and pickers, over whatever is on offer.
 *
 * Every row is conditional on the offering holding something of that kind, which
 * is what lets one set of fields serve three places: a backup, a restore (where
 * a file with no applications in it simply has no applications row to untick),
 * and an automated backup's dialog. `idPrefix` keeps the controls' ids apart
 * between them. */
function IncludeFields({
  idPrefix,
  contents,
  selection,
  onChange,
}: {
  idPrefix: string;
  contents: BackupContents;
  selection: BackupSelection;
  onChange: (selection: BackupSelection) => void;
}) {
  const { t } = useT();
  return (
    <>
      {contents.tables.length > 0 && (
        <>
          <Form.Group className="mb-3" controlId={`${idPrefix}-tables`}>
            <Form.Label><T text="Table definitions" /></Form.Label>
            <MultiSelect
              id={`${idPrefix}-tables`}
              options={choices(contents.tables, "row")}
              selected={selection.tables}
              onChange={(tables) => onChange(withTables(selection, tables))}
              placeholder={t("No tables")}
            />
            <Form.Text muted>
              <T text="A table's columns, its access rules and its ownership formula." />
            </Form.Text>
          </Form.Group>

          <Form.Group className="mb-3" controlId={`${idPrefix}-table-data`}>
            <Form.Label><T text="Table data" /></Form.Label>
            <MultiSelect
              id={`${idPrefix}-table-data`}
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
        <Form.Group className="mb-3" controlId={`${idPrefix}-applications`}>
          <Form.Label><T text="Applications" /></Form.Label>
          <MultiSelect
            id={`${idPrefix}-applications`}
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
        <Form.Group className="mb-3" controlId={`${idPrefix}-file-stores`}>
          <Form.Label><T text="Files" /></Form.Label>
          <MultiSelect
            id={`${idPrefix}-file-stores`}
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
            id={`${idPrefix}-users`}
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
            id={`${idPrefix}-modules`}
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
            id={`${idPrefix}-db-connections`}
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
            id={`${idPrefix}-streams`}
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
            id={`${idPrefix}-analytics`}
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
            id={`${idPrefix}-fits`}
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
            id={`${idPrefix}-llm-providers`}
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
            id={`${idPrefix}-agents`}
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
            id={`${idPrefix}-triggers`}
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
            id={`${idPrefix}-views`}
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
            id={`${idPrefix}-pages`}
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
            id={`${idPrefix}-ssl`}
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
            id={`${idPrefix}-settings`}
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
    </>
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

/** The Automated backups card: the recurring backups, one line each, with an
 * Add button and an Edit/Delete pair per line, edited in a modal.
 *
 * Each schedule has its own selection, edited with the same fields as the
 * backup dialog (`IncludeFields`). What there is to choose from is the Backup
 * card's `contents`, and a new schedule starts from the card's current
 * selection (`cardSelection`) — the choice the admin has already made once. */
function AutomatedBackups({
  contents,
  cardSelection,
}: {
  contents: BackupContents;
  cardSelection: BackupSelection | null;
}) {
  const { t } = useT();
  const [schedules, setSchedules] = useState<BackupSchedule[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<ScheduleForm | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [submitted, setSubmitted] = useState(false);
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    try {
      setSchedules(await api.listBackupSchedules());
    } catch (e) {
      setError(errorMessage(e, "Could not read the automated backups."));
    }
  }, []);

  // Read again when what there is to back up changes (a restore, a backup just
  // taken): each schedule's selection comes back resolved against it.
  useEffect(() => {
    void load();
  }, [load, contents]);

  const open = (form: ScheduleForm) => {
    setEditing(form);
    setFormError(null);
    setSubmitted(false);
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing) return;
    setSubmitted(true);
    if (Object.keys(scheduleFormErrors(editing)).length > 0) return;
    setSaving(true);
    setFormError(null);
    try {
      const body = scheduleBody(editing);
      if (editing.id) await api.updateBackupSchedule(editing.id, body);
      else await api.createBackupSchedule(body);
      setEditing(null);
      await load();
    } catch (err) {
      // Shown in the dialog, not behind it: the server's refusals — a
      // directory it cannot write, a password the SFTP server refused, a
      // bucket another schedule already uses — are about what was just typed.
      setFormError(errorMessage(err, "Could not save the automated backup."));
    } finally {
      setSaving(false);
    }
  };

  const remove = async (schedule: BackupSchedule) => {
    if (
      !window.confirm(
        t("Stop the automated backup to {destination}? The backups already there are kept.", {
          destination: schedule.location,
        }),
      )
    )
      return;
    setError(null);
    try {
      await api.deleteBackupSchedule(schedule.id);
      await load();
    } catch (e) {
      setError(errorMessage(e, "Could not delete the automated backup."));
    }
  };

  const errors = editing && submitted ? scheduleFormErrors(editing, t) : {};

  return (
    <div className="card">
      <div className="card-header">
        <div>
          <h3 className="card-title"><T text="Automated backups" /></h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="Backups sent on a schedule to a directory on the server, an SFTP server or an S3-compatible bucket, each with its own choice of what to include. Backups there older than the retention period are deleted." />
          </p>
        </div>
        <div className="card-actions">
          <Button
            disabled={cardSelection === null}
            onClick={() => cardSelection && open(newScheduleForm(cardSelection))}
          >
            <IconPlus /> <T text="Add" />
          </Button>
        </div>
      </div>
      {error && (
        <div className="card-body pb-0">
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        </div>
      )}
      {schedules === null ? (
        <div className="card-body">
          <Spinner animation="border" role="status" size="sm" />
        </div>
      ) : schedules.length === 0 ? (
        <div className="card-body">
          <p className="text-secondary mb-0">
            <T text="No automated backups. Add one to have this server back itself up every day or every week." />
          </p>
        </div>
      ) : (
        <Table responsive className="card-table table-vcenter mb-0">
          <thead>
            <tr>
              <th><T text="Destination" /></th>
              <th><T text="Frequency" /></th>
              <th><T text="Retention" /></th>
              <th><T text="Status" /></th>
              <th />
            </tr>
          </thead>
          <tbody>
            {schedules.map((schedule) => {
              const status = scheduleStatus(schedule);
              return (
                <tr key={schedule.id}>
                  <td>
                    <div>
                      <span className="badge bg-secondary-lt me-2">
                        {destinationKindLabel(schedule.destination.kind, t)}
                      </span>
                      <span className="font-monospace">{schedule.location}</span>
                    </div>
                    {schedule.destination.host_key && (
                      <div className="small text-secondary">
                        {t("Host key {fingerprint}", {
                          fingerprint: schedule.destination.host_key,
                        })}
                      </div>
                    )}
                    <div className="small text-secondary">
                      {t("Includes: {summary}", {
                        summary: summarise(schedule.include, contents),
                      })}
                    </div>
                  </td>
                  <td>{frequencyLabel(schedule.frequency, t)}</td>
                  <td>{t("{count} days", { count: schedule.retention_days })}</td>
                  <td className={status.failed ? "text-danger small" : "text-secondary small"}>
                    {status.text}
                  </td>
                  <td className="text-end">
                    <div className="btn-list flex-nowrap justify-content-end">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        aria-label={t("Edit")}
                        onClick={() => open(editScheduleForm(schedule))}
                      >
                        <IconPencil className="icon-2" /> <T text="Edit" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        aria-label={t("Delete")}
                        onClick={() => void remove(schedule)}
                      >
                        <IconTrash className="icon-2" /> <T text="Delete" />
                      </Button>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </Table>
      )}

      <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg" scrollable>
        {editing && (
          <Form onSubmit={(e) => void save(e)} noValidate>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {editing.id ? t("Edit automated backup") : t("Add automated backup")}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {formError && (
                <Alert variant="danger">
                  <AlertBody>{formError}</AlertBody>
                </Alert>
              )}
              <DestinationFields
                form={editing}
                errors={errors}
                onChange={(change) => setEditing({ ...editing, ...change })}
              />
              <Form.Group className="mb-3" controlId="schedule-frequency">
                <Form.Label><T text="Frequency" /></Form.Label>
                <Form.Select
                  value={editing.frequency}
                  onChange={(e) =>
                    setEditing({ ...editing, frequency: e.target.value as Frequency })
                  }
                >
                  {FREQUENCIES.map((f) => (
                    <option key={f} value={f}>
                      {frequencyLabel(f, t)}
                    </option>
                  ))}
                </Form.Select>
              </Form.Group>
              <Form.Group className="mb-4" controlId="schedule-retention">
                <Form.Label><T text="Retention (days)" /></Form.Label>
                <Form.Control
                  type="number"
                  min={1}
                  max={MAX_RETENTION_DAYS}
                  step={1}
                  value={editing.retention}
                  isInvalid={errors.retention !== undefined}
                  onChange={(e) => setEditing({ ...editing, retention: e.target.value })}
                />
                <Form.Control.Feedback type="invalid">
                  {errors.retention}
                </Form.Control.Feedback>
                <Form.Text muted>
                  <T text="Backups at the destination older than this are deleted after each new backup." />
                </Form.Text>
              </Form.Group>

              <h4 className="mb-1"><T text="What each backup includes" /></h4>
              <p className="text-secondary small mb-3">
                <T text="Anything created later, such as a new table, is included unless you untick it here." />
              </p>
              {errors.include && (
                <Alert variant="danger">
                  <AlertBody>{errors.include}</AlertBody>
                </Alert>
              )}
              <IncludeFields
                idPrefix="schedule-include"
                contents={contents}
                selection={editing.include}
                onChange={(include) => setEditing({ ...editing, include })}
              />
            </Modal.Body>
            <Modal.Footer>
              <Button variant="secondary" type="button" onClick={() => setEditing(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={saving}>
                {saving ? t("Saving…") : t("Save")}
              </Button>
            </Modal.Footer>
          </Form>
        )}
      </Modal>
    </div>
  );
}

/** The top of the schedule dialog: where the backups go — local files, an
 * SFTP server or an S3-compatible bucket — and that kind's own fields. */
function DestinationFields({
  form,
  errors,
  onChange,
}: {
  form: ScheduleForm;
  errors: Partial<Record<ScheduleField, string>>;
  onChange: (change: Partial<ScheduleForm>) => void;
}) {
  const { t } = useT();
  /** A text box for one of the form's fields, with its error. */
  const field = (
    name: ScheduleField & keyof ScheduleForm,
    label: string,
    options: {
      placeholder?: string;
      help?: string;
      required?: boolean;
      monospace?: boolean;
      type?: string;
    } = {},
  ) => (
    <Form.Group className="mb-3" controlId={`schedule-${name}`}>
      <Form.Label>
        {label}
        {options.required && <span className="text-danger"> *</span>}
      </Form.Label>
      <Form.Control
        className={options.monospace ? "font-monospace" : undefined}
        type={options.type ?? "text"}
        placeholder={options.placeholder}
        value={String(form[name] ?? "")}
        isInvalid={errors[name] !== undefined}
        onChange={(e) => onChange({ [name]: e.target.value })}
      />
      <Form.Control.Feedback type="invalid">{errors[name]}</Form.Control.Feedback>
      {options.help && <Form.Text muted>{options.help}</Form.Text>}
    </Form.Group>
  );
  /** A password box holding a stored secret as the mask: cleared on focus so
   * typing replaces it rather than appending to it, and put back on blur if
   * nothing was typed, so tabbing through the form keeps the stored one. */
  const secret = (name: "password" | "secretKey", label: string) => (
    <Form.Group className="mb-3" controlId={`schedule-${name}`}>
      <Form.Label>
        {label}
        <span className="text-danger"> *</span>
      </Form.Label>
      <Form.Control
        type="password"
        autoComplete="new-password"
        value={form[name]}
        isInvalid={errors[name] !== undefined}
        onChange={(e) => onChange({ [name]: e.target.value })}
        onFocus={() => form.id && isStoredSecret(form[name]) && onChange({ [name]: "" })}
        onBlur={() => form.id && form[name] === "" && onChange({ [name]: SECRET_SENTINEL })}
      />
      <Form.Control.Feedback type="invalid">{errors[name]}</Form.Control.Feedback>
      {isStoredSecret(form[name]) && (
        <Form.Text muted><T text="Stored. Type to replace it." /></Form.Text>
      )}
    </Form.Group>
  );

  return (
    <>
      <Form.Group className="mb-3" controlId="schedule-kind">
        <Form.Label><T text="Destination" /></Form.Label>
        <Form.Select
          value={form.kind}
          onChange={(e) => onChange({ kind: e.target.value as DestinationKind })}
        >
          {DESTINATION_KINDS.map((kind) => (
            <option key={kind} value={kind}>
              {destinationKindLabel(kind, t)}
            </option>
          ))}
        </Form.Select>
      </Form.Group>

      {form.kind === "local" &&
        field("directory", t("Directory"), {
          required: true,
          monospace: true,
          placeholder: "/var/backups/feldspar",
          help: t(
            "An absolute path to a directory on the server. It is created if it does not exist, and no other automated backup may use it.",
          ),
        })}

      {form.kind === "sftp" && (
        <>
          <div className="row">
            <div className="col-sm-8">
              {field("host", t("Host"), {
                required: true,
                monospace: true,
                placeholder: "backup.example.com",
              })}
            </div>
            <div className="col-sm-4">
              {field("port", t("Port"), {
                type: "number",
                placeholder: String(DEFAULT_SFTP_PORT),
              })}
            </div>
          </div>
          <div className="row">
            <div className="col-sm-6">{field("username", t("User name"), { required: true })}</div>
            <div className="col-sm-6">{secret("password", t("Password"))}</div>
          </div>
          {field("remoteDirectory", t("Directory"), {
            monospace: true,
            placeholder: "backups/feldspar",
            help: t(
              "Absolute, or relative to where the login starts; empty for that directory itself. It is created if it does not exist.",
            ),
          })}
          {form.hostKey ? (
            <p className="small text-secondary">
              {t(
                "Host key {fingerprint}, recorded when this was last saved. Saving again accepts the key the server presents now.",
                { fingerprint: form.hostKey },
              )}
            </p>
          ) : (
            <p className="small text-secondary">
              <T text="Saving logs in to check the server and records its host key. A later backup is refused if the server presents a different key." />
            </p>
          )}
        </>
      )}

      {form.kind === "s3" && (
        <>
          {field("endpoint", t("Endpoint"), {
            monospace: true,
            placeholder: "https://s3.eu-west-1.amazonaws.com",
            help: t(
              "The URL of any S3-compatible service, such as MinIO, Cloudflare R2 or Backblaze B2. Leave it empty for Amazon S3.",
            ),
          })}
          <div className="row">
            <div className="col-sm-6">
              {field("bucket", t("Bucket"), { required: true, monospace: true })}
            </div>
            <div className="col-sm-6">
              {field("region", t("Region"), { monospace: true, placeholder: "us-east-1" })}
            </div>
          </div>
          <div className="row">
            <div className="col-sm-6">
              {field("accessKey", t("Access key"), { required: true, monospace: true })}
            </div>
            <div className="col-sm-6">{secret("secretKey", t("Secret key"))}</div>
          </div>
          <p className="small text-secondary">
            <T text="Saving writes and deletes a test object to check the keys. No other automated backup may use the same bucket." />
          </p>
        </>
      )}
    </>
  );
}
