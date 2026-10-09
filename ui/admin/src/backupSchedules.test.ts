/**
 * The Automated backups card's model: the form's checks, the body it sends,
 * and the status line each schedule shows.
 */

import { describe, expect, it } from "vitest";

import { NO_CONTENTS, everything, type BackupSelection } from "./backup";
import {
  FREQUENCIES,
  destinationKindLabel,
  editScheduleForm,
  frequencyLabel,
  newScheduleForm,
  scheduleBody,
  scheduleFormErrors,
  scheduleStatus,
  type BackupSchedule,
  type ScheduleForm,
} from "./backupSchedules";
import { SECRET_SENTINEL } from "./dbConnection";

const all: BackupSelection = everything({
  ...NO_CONTENTS,
  tables: [{ name: "books", label: "books", count: 3 }],
  users: 2,
});
const nothing: BackupSelection = { ...all, tables: [], table_data: [], users: false };

const schedule: BackupSchedule = {
  id: "6f9619ff-8b86-d011-b42d-00c04fc964ff",
  destination: { kind: "local", directory: "/srv/backups" },
  location: "/srv/backups",
  frequency: "weekly",
  retention_days: 30,
  include: all,
  last_attempt_at: null,
  last_success_at: null,
  last_error: null,
  last_file: null,
};

const sftpSchedule: BackupSchedule = {
  ...schedule,
  destination: {
    kind: "sftp",
    host: "backup.example.com",
    port: 2222,
    username: "feldspar",
    password: SECRET_SENTINEL,
    directory: "backups",
    host_key: "SHA256:abc",
  },
  location: "sftp://feldspar@backup.example.com:2222/~/backups",
};

const s3Schedule: BackupSchedule = {
  ...schedule,
  destination: {
    kind: "s3",
    endpoint: "https://minio.example.com",
    bucket: "site",
    region: "",
    access_key: "AK",
    secret_key: SECRET_SENTINEL,
  },
  location: "https://minio.example.com/site",
};

/** A form that would be sent, of each kind. */
const local: ScheduleForm = { ...newScheduleForm(all), directory: "/srv/b", retention: "7" };
const sftp: ScheduleForm = {
  ...local,
  kind: "sftp",
  host: "backup.example.com",
  username: "feldspar",
  password: "hunter2",
};
const s3: ScheduleForm = { ...local, kind: "s3", bucket: "site", accessKey: "AK", secretKey: "SK" };

describe("the schedule form", () => {
  it("opens on local files for a new schedule", () => {
    expect(newScheduleForm(all)).toMatchObject({
      id: null,
      kind: "local",
      directory: "",
      port: "22",
      frequency: "daily",
      include: all,
    });
  });

  it("opens on each kind's fields for an edit, the secret as the mask", () => {
    expect(editScheduleForm(schedule)).toMatchObject({
      id: schedule.id,
      kind: "local",
      directory: "/srv/backups",
      frequency: "weekly",
      retention: "30",
      include: all,
    });
    expect(editScheduleForm(sftpSchedule)).toMatchObject({
      kind: "sftp",
      directory: "",
      host: "backup.example.com",
      port: "2222",
      username: "feldspar",
      password: SECRET_SENTINEL,
      remoteDirectory: "backups",
      hostKey: "SHA256:abc",
    });
    expect(editScheduleForm(s3Schedule)).toMatchObject({
      kind: "s3",
      endpoint: "https://minio.example.com",
      bucket: "site",
      accessKey: "AK",
      secretKey: SECRET_SENTINEL,
    });
  });

  it("keeps a monthly schedule monthly, and offers and names the frequency", () => {
    expect(editScheduleForm({ ...schedule, frequency: "monthly" }).frequency).toBe("monthly");
    expect(scheduleBody({ ...local, frequency: "monthly", retention: "90" }).frequency).toBe(
      "monthly",
    );
    expect(FREQUENCIES).toContain("monthly");
    expect(frequencyLabel("monthly")).toBe("Monthly");
  });

  it("wants an absolute directory for local files", () => {
    expect(scheduleFormErrors({ ...local, directory: "" }).directory).toBeDefined();
    expect(scheduleFormErrors({ ...local, directory: "backups" }).directory).toBeDefined();
    expect(scheduleFormErrors({ ...local, directory: "/srv/../etc" }).directory).toBeDefined();
    expect(scheduleFormErrors(local)).toEqual({});
  });

  it("checks only the chosen kind's fields", () => {
    // The local directory is empty in the SFTP and S3 forms, and that is fine.
    expect(scheduleFormErrors({ ...sftp, directory: "" })).toEqual({});
    expect(scheduleFormErrors({ ...s3, directory: "" })).toEqual({});
  });

  it("wants a host, a user name, a password and a sensible port for SFTP", () => {
    expect(scheduleFormErrors({ ...sftp, host: "" }).host).toBeDefined();
    expect(scheduleFormErrors({ ...sftp, host: "me@backup" }).host).toBeDefined();
    expect(scheduleFormErrors({ ...sftp, username: " " }).username).toBeDefined();
    expect(scheduleFormErrors({ ...sftp, password: "" }).password).toBeDefined();
    for (const bad of ["0", "70000", "ssh"])
      expect(scheduleFormErrors({ ...sftp, port: bad }).port).toBeDefined();
    expect(scheduleFormErrors({ ...sftp, port: "" })).toEqual({});
    expect(scheduleFormErrors({ ...sftp, remoteDirectory: "a/../b" }).remoteDirectory).toBeDefined();
    // The stored password, untouched, is a password.
    expect(scheduleFormErrors({ ...sftp, password: SECRET_SENTINEL })).toEqual({});
  });

  it("wants a bucket and keys for S3, and an endpoint only if it is a URL", () => {
    expect(scheduleFormErrors({ ...s3, bucket: "" }).bucket).toBeDefined();
    expect(scheduleFormErrors({ ...s3, bucket: "a/b" }).bucket).toBeDefined();
    expect(scheduleFormErrors({ ...s3, accessKey: "" }).accessKey).toBeDefined();
    expect(scheduleFormErrors({ ...s3, secretKey: "" }).secretKey).toBeDefined();
    expect(scheduleFormErrors({ ...s3, endpoint: "minio:9000" }).endpoint).toBeDefined();
    expect(scheduleFormErrors({ ...s3, endpoint: "http://localhost:9000" })).toEqual({});
  });

  it("wants a whole number of days of at least one", () => {
    for (const bad of ["", "0", "-3", "1.5", "seven", "99999"])
      expect(scheduleFormErrors({ ...local, retention: bad }).retention).toBeDefined();
    expect(scheduleFormErrors({ ...local, retention: " 14 " })).toEqual({});
  });

  it("wants something to include", () => {
    const form = { ...local, include: nothing };
    expect(scheduleFormErrors(form).include).toBeDefined();
    expect(scheduleFormErrors({ ...form, include: all })).toEqual({});
  });

  it("sends the chosen kind's fields, trimmed, a number of days and the selection", () => {
    expect(
      scheduleBody({ ...local, directory: " /srv/b ", frequency: "weekly", retention: "14" }),
    ).toEqual({
      destination: { kind: "local", directory: "/srv/b" },
      frequency: "weekly",
      retention_days: 14,
      include: all,
    });
    expect(
      scheduleBody({ ...sftp, port: "", password: " pass word ", remoteDirectory: " b/ " })
        .destination,
    ).toEqual({
      kind: "sftp",
      host: "backup.example.com",
      port: 22,
      username: "feldspar",
      // Sent as typed: spaces can be part of a password.
      password: " pass word ",
      directory: "b/",
    });
    expect(scheduleBody({ ...s3, region: " eu-west-1 " }).destination).toEqual({
      kind: "s3",
      endpoint: "",
      bucket: "site",
      region: "eu-west-1",
      access_key: "AK",
      secret_key: "SK",
    });
  });

  it("sends the mask back unchanged when the secret was not retyped", () => {
    const edit = editScheduleForm(sftpSchedule);
    expect(scheduleBody(edit).destination).toMatchObject({ password: SECRET_SENTINEL });
    // The host key is the server's to record, never the form's to send.
    expect(scheduleBody(edit).destination).not.toHaveProperty("host_key");
  });

  it("names the kinds", () => {
    expect(destinationKindLabel("local")).toBe("Local files");
    expect(destinationKindLabel("sftp")).toBe("SFTP");
    expect(destinationKindLabel("s3")).toBe("S3");
  });
});

describe("a schedule's status line", () => {
  const time = (iso: string) => iso.slice(0, 10);

  it("says when it has not run yet", () => {
    expect(scheduleStatus(schedule, time)).toMatchObject({ failed: false });
    expect(scheduleStatus(schedule, time).text).toMatch(/Not run yet/);
  });

  it("says when it last succeeded", () => {
    const ran = { ...schedule, last_success_at: "2026-10-01T02:00:00Z" };
    expect(scheduleStatus(ran, time)).toEqual({ text: "Last backup 2026-10-01", failed: false });
  });

  it("says why it last failed, ahead of an earlier success", () => {
    const failed = {
      ...schedule,
      last_success_at: "2026-10-01T02:00:00Z",
      last_attempt_at: "2026-10-02T02:00:00Z",
      last_error: "disk full",
    };
    expect(scheduleStatus(failed, time)).toEqual({
      text: "Failed 2026-10-02: disk full",
      failed: true,
    });
  });
});
