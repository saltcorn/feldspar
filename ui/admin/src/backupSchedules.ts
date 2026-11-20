/**
 * The Automated backups card's model: the form a schedule is edited in, what
 * it checks before it is sent, and what a schedule's line says about it.
 *
 * A schedule's destination is a directory on the server, a directory on an
 * SFTP server, or an S3-compatible bucket. The form keeps every kind's fields
 * at once, so switching the kind back and forth does not lose what was typed;
 * only the chosen kind's fields are checked and sent.
 *
 * A schedule's `include` is the same selection the backup dialog edits
 * (`backup.ts` has its arithmetic); the server stores it as what was left out.
 * Beside it, `includeAll` says which kinds — tables, applications, file stores —
 * are included whole: every one there is when the backup runs, so one created
 * after the schedule was set up is in it whatever the lists say.
 *
 * A schedule can also name who is emailed when one of its backups fails:
 * nobody, every administrator ("all") or one administrator by address. The
 * choice is offered only when the installation sends mail.
 *
 * The server checks everything again (and checks what only it can — that the
 * destination can be reached and written, that no other schedule writes
 * there); these checks are here so the obvious mistakes are reported beside
 * the field rather than after a round trip.
 */

import { isEmpty, type BackupSelection } from "./backup";
import type { ListBackupSchedulesResponse } from "./client";
import { SECRET_SENTINEL } from "./dbConnection";
import { format, type Translator } from "./i18n";

export type BackupSchedule = ListBackupSchedulesResponse[number];

export type Frequency = "daily" | "weekly" | "monthly";

export const FREQUENCIES: Frequency[] = ["daily", "weekly", "monthly"];

/** The kinds a schedule includes every one of, at the time it runs. */
export type IncludeAll = { tables: boolean; applications: boolean; file_stores: boolean };

/** What a new schedule starts on: everything, now and later. */
export const INCLUDE_ALL: IncludeAll = { tables: true, applications: true, file_stores: true };

/** None: the lists decide for every kind. */
export const INCLUDE_LISTED: IncludeAll = {
  tables: false,
  applications: false,
  file_stores: false,
};

export type DestinationKind = "local" | "sftp" | "s3";

export const DESTINATION_KINDS: DestinationKind[] = ["local", "sftp", "s3"];

/** The most days a backup can be kept — the server's `MAX_RETENTION_DAYS`. */
export const MAX_RETENTION_DAYS = 3650;

/** The port SFTP listens on unless told otherwise. */
export const DEFAULT_SFTP_PORT = 22;

/** The schedule being added or edited, as the form holds it: `id` is null for
 * a new one, and numbers are the text in their boxes. A stored password or
 * secret key arrives as `SECRET_SENTINEL`, and sending it back unchanged keeps
 * what the server has. */
export type ScheduleForm = {
  id: string | null;
  kind: DestinationKind;
  /** Local: an absolute directory on the server. */
  directory: string;
  /** SFTP. */
  host: string;
  port: string;
  username: string;
  password: string;
  remoteDirectory: string;
  /** SFTP: the server's host key fingerprint, recorded by the server when the
   * schedule was last saved; shown, never sent. */
  hostKey: string | null;
  /** S3. */
  endpoint: string;
  bucket: string;
  region: string;
  accessKey: string;
  secretKey: string;
  frequency: Frequency;
  retention: string;
  include: BackupSelection;
  includeAll: IncludeAll;
  /** The password each backup is encrypted with: empty for none, and
   * `SECRET_SENTINEL` while the stored one is kept. */
  backupPassword: string;
  /** The password typed again; checked only for one that was typed. */
  backupPasswordRepeat: string;
  /** Whether the server holds a password for this schedule that the form has
   * not removed. */
  backupPasswordStored: boolean;
  /** Who is emailed when a backup fails: empty for nobody, `FAILURE_EMAIL_ALL`
   * for every administrator, or one administrator's address. */
  failureEmail: string;
};

/** `failure_email`'s value for every administrator. */
export const FAILURE_EMAIL_ALL = "all";

export type ScheduleField =
  | "directory"
  | "host"
  | "port"
  | "username"
  | "password"
  | "remoteDirectory"
  | "endpoint"
  | "bucket"
  | "region"
  | "accessKey"
  | "secretKey"
  | "retention"
  | "include"
  | "backupPassword";

/** What the Add button opens on: what to include starts as the Backup card's
 * current selection, the one choice the admin has already made, with all
 * tables, applications and file stores — including those created later. */
export function newScheduleForm(include: BackupSelection): ScheduleForm {
  return {
    id: null,
    kind: "local",
    directory: "",
    host: "",
    port: String(DEFAULT_SFTP_PORT),
    username: "",
    password: "",
    remoteDirectory: "",
    hostKey: null,
    endpoint: "",
    bucket: "",
    region: "",
    accessKey: "",
    secretKey: "",
    frequency: "daily",
    retention: "30",
    include,
    includeAll: INCLUDE_ALL,
    backupPassword: "",
    backupPasswordRepeat: "",
    backupPasswordStored: false,
    failureEmail: "",
  };
}

/** What the Edit button opens on. */
export function editScheduleForm(schedule: BackupSchedule): ScheduleForm {
  const d = schedule.destination;
  const kind: DestinationKind = d.kind === "sftp" || d.kind === "s3" ? d.kind : "local";
  const blank = newScheduleForm(schedule.include);
  return {
    ...blank,
    id: schedule.id,
    kind,
    directory: kind === "local" ? (d.directory ?? "") : "",
    host: d.host ?? "",
    port: d.port != null ? String(d.port) : blank.port,
    username: d.username ?? "",
    password: d.password ?? "",
    remoteDirectory: kind === "sftp" ? (d.directory ?? "") : "",
    hostKey: d.host_key ?? null,
    endpoint: d.endpoint ?? "",
    bucket: d.bucket ?? "",
    region: d.region ?? "",
    accessKey: d.access_key ?? "",
    secretKey: d.secret_key ?? "",
    frequency: (FREQUENCIES as string[]).includes(schedule.frequency)
      ? (schedule.frequency as Frequency)
      : "daily",
    retention: String(schedule.retention_days),
    includeAll: { ...INCLUDE_ALL, ...schedule.include_all },
    backupPassword: schedule.password ? SECRET_SENTINEL : "",
    backupPasswordStored: !!schedule.password,
    failureEmail: schedule.failure_email ?? "",
  };
}

/** Why the form cannot be sent, by field; empty when it can. Only the chosen
 * kind's fields are checked. The screen passes its `t`, so each message is a
 * literal the extractor finds here; without one the messages are the
 * English. */
export function scheduleFormErrors(
  form: ScheduleForm,
  t: Translator["t"] = format,
): Partial<Record<ScheduleField, string>> {
  const errors: Partial<Record<ScheduleField, string>> = {};
  if (form.kind === "local") {
    const directory = form.directory.trim();
    if (directory === "") errors.directory = t("Enter a directory on the server.");
    else if (!directory.startsWith("/"))
      errors.directory = t("Enter an absolute path, starting with /.");
    else if (directory.split("/").includes(".."))
      errors.directory = t("Enter the path without '..'.");
  } else if (form.kind === "sftp") {
    const host = form.host.trim();
    if (host === "") errors.host = t("Enter the SFTP server's host name.");
    else if (/[\s/@]/.test(host))
      errors.host = t("Enter the host name alone, without a user name or path.");
    const port = form.port.trim();
    if (port !== "" && (!/^\d+$/.test(port) || Number(port) < 1 || Number(port) > 65535))
      errors.port = t("Enter a port from 1 to 65535.");
    if (form.username.trim() === "") errors.username = t("Enter the user name.");
    if (form.password === "") errors.password = t("Enter the password.");
    if (form.remoteDirectory.split("/").includes(".."))
      errors.remoteDirectory = t("Enter the path without '..'.");
  } else {
    const endpoint = form.endpoint.trim();
    if (endpoint !== "" && !/^https?:\/\/[^\s/]+/.test(endpoint))
      errors.endpoint = t("Enter the endpoint as a URL, such as https://s3.example.com.");
    const bucket = form.bucket.trim();
    if (bucket === "") errors.bucket = t("Enter the bucket name.");
    else if (/[\s/]/.test(bucket)) errors.bucket = t("Enter the bucket's name alone, without /.");
    if (form.accessKey.trim() === "") errors.accessKey = t("Enter the access key.");
    if (form.secretKey.trim() === "") errors.secretKey = t("Enter the secret key.");
  }
  const retention = form.retention.trim();
  const days = Number(retention);
  if (!/^\d+$/.test(retention) || days < 1 || days > MAX_RETENTION_DAYS)
    errors.retention = t("Enter a whole number of days from 1 to {max}.", {
      max: MAX_RETENTION_DAYS,
    });
  if (!includesAny(form.includeAll) && isEmpty(form.include)) errors.include = t("Choose at least one thing to include.");
  if (
    form.backupPassword !== "" &&
    !isStoredSecret(form.backupPassword) &&
    form.backupPassword !== form.backupPasswordRepeat
  )
    errors.backupPassword = t("The passwords do not match.");
  return errors;
}

/** The destination the form describes, in the API's shape: the chosen kind's
 * fields only. A password is sent as typed (not trimmed). */
export function destinationBody(form: ScheduleForm) {
  switch (form.kind) {
    case "local":
      return { kind: "local", directory: form.directory.trim() };
    case "sftp":
      return {
        kind: "sftp",
        host: form.host.trim(),
        port: form.port.trim() === "" ? DEFAULT_SFTP_PORT : Number(form.port.trim()),
        username: form.username.trim(),
        password: form.password,
        directory: form.remoteDirectory.trim(),
      };
    case "s3":
      return {
        kind: "s3",
        endpoint: form.endpoint.trim(),
        bucket: form.bucket.trim(),
        region: form.region.trim(),
        access_key: form.accessKey.trim(),
        secret_key: form.secretKey.trim(),
      };
  }
}

/** The request body the form becomes. Only call it on a form with no errors. */
export function scheduleBody(form: ScheduleForm) {
  return {
    destination: destinationBody(form),
    frequency: form.frequency,
    retention_days: Number(form.retention.trim()),
    include: form.include,
    include_all: form.includeAll,
    // The mask keeps the stored password; empty is none, which removes it.
    password: form.backupPassword === "" ? null : form.backupPassword,
    failure_email: form.failureEmail === "" ? null : form.failureEmail,
  };
}

/** Whether any kind is included whole — which is never nothing, since what it
 * covers may be created after the schedule is saved. */
export function includesAny(all: IncludeAll): boolean {
  return all.tables || all.applications || all.file_stores;
}

/** What a schedule's line says about the kinds it includes whole, or null for
 * none: "New tables and file stores are included automatically." */
export function includeAllNote(all: IncludeAll | null | undefined): string | null {
  const kinds = [
    all?.tables && "tables",
    all?.applications && "applications",
    all?.file_stores && "file stores",
  ].filter((k): k is string => !!k);
  if (kinds.length === 0) return null;
  const list =
    kinds.length === 1 ? kinds[0] : `${kinds.slice(0, -1).join(", ")} and ${kinds[kinds.length - 1]}`;
  return `New ${list} are included automatically.`;
}

/** Whether a secret box holds the stored value rather than something typed. */
export function isStoredSecret(value: string): boolean {
  return value === SECRET_SENTINEL;
}

/** "Local files", "SFTP" or "S3", in the screen's language when it passes its
 * `t`. */
export function destinationKindLabel(kind: string, t: Translator["t"] = format): string {
  switch (kind) {
    case "local":
      return t("Local files");
    case "sftp":
      return t("SFTP");
    case "s3":
      return t("S3");
    default:
      return kind;
  }
}

/** "Daily", "Weekly" or "Monthly", in the screen's language when it passes its `t`. */
export function frequencyLabel(frequency: string, t: Translator["t"] = format): string {
  switch (frequency) {
    case "daily":
      return t("Daily");
    case "weekly":
      return t("Weekly");
    case "monthly":
      return t("Monthly");
    default:
      return frequency;
  }
}

/** What a schedule last did, in one line, and whether it is a problem. */
export function scheduleStatus(
  schedule: BackupSchedule,
  formatTime: (iso: string) => string = (iso) => new Date(iso).toLocaleString(),
): { text: string; failed: boolean } {
  if (schedule.last_error && schedule.last_attempt_at)
    return {
      text: `Failed ${formatTime(schedule.last_attempt_at)}: ${schedule.last_error}`,
      failed: true,
    };
  if (schedule.last_success_at)
    return { text: `Last backup ${formatTime(schedule.last_success_at)}`, failed: false };
  return { text: "Not run yet — the first backup is taken within a minute.", failed: false };
}

/** The failure email dropdown's choices: nobody, every administrator, then each
 * administrator. An address the schedule names that is no longer an
 * administrator's is kept as a choice, so opening the dialog does not quietly
 * change it (the server refuses to save it, and says why). */
export function failureEmailOptions(
  admins: string[],
  current: string,
  t: Translator["t"] = format,
): { value: string; label: string }[] {
  const options = [
    { value: "", label: t("None") },
    { value: FAILURE_EMAIL_ALL, label: t("All administrators") },
    ...admins.map((a) => ({ value: a, label: a })),
  ];
  if (!options.some((o) => o.value.toLowerCase() === current.toLowerCase()))
    options.push({ value: current, label: current });
  return options;
}

/** What a schedule's line says about its failure email, or null for none. */
export function failureEmailNote(
  failureEmail: string | null | undefined,
  t: Translator["t"] = format,
): string | null {
  if (!failureEmail) return null;
  if (failureEmail === FAILURE_EMAIL_ALL) return t("Failures emailed to all administrators");
  return t("Failures emailed to {address}", { address: failureEmail });
}
