//! The admin API's endpoints: backup and restore, automated backups and clear all.
//!
//! `sc-server`'s `handlers::backup` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Two of the four backup operations are here; the other two are routes outside
    // this set, because one *is* a file and the other *takes* one, and a
    // `TypeSchema` has no bytes shape (the same reason the binary file upload is
    // outside it). The split is along that line and no other: what an admin
    // includes, and what a restore did, are ordinary typed JSON and belong in the
    // generated client.

    // What this server has to offer a backup, and the selection the admin last
    // made — one response, because the dialog needs both to open and asking for
    // them separately would let them disagree.
    set.register(
        Endpoint::new("getBackupOptions", Method::Get, api().lit("backup"))
            .output(TypeSchema::struct_of([
                StructField::new("available", backup_contents_schema()),
                StructField::new("include", backup_selection_schema()),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The password of an uploaded backup that is protected by one: the upload
    // answered `locked` and its `id`, and this answers what the upload would
    // have — what the file holds — once the password opens it. A wrong one is
    // refused and the file stays held, to be tried again; the right one is
    // held with it for `restoreBackup`.
    set.register(
        Endpoint::new(
            "unlockBackup",
            Method::Post,
            api().lit("backup").lit("unlock"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::text()),
            StructField::new("password", TypeSchema::text()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::text()),
            StructField::new("encrypted", TypeSchema::bool()),
            StructField::new("locked", TypeSchema::bool()),
            StructField::new("created_at", TypeSchema::optional(TypeSchema::text())),
            StructField::new("source", TypeSchema::optional(TypeSchema::text())),
            StructField::new("available", backup_contents_schema()),
            StructField::new("include", backup_selection_schema()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Restore from an archive already uploaded (`/backup/upload` handed back its
    // `id`), taking the parts `include` names.
    //
    // The two lists rather than a single "ok": a restore is dozens of independent
    // acts and some of them are routinely skipped — an account that is already
    // here, a trigger on a table the admin left out. Reporting that as success
    // would hide it and as failure would be wrong.
    set.register(
        Endpoint::new(
            "restoreBackup",
            Method::Post,
            api().lit("backup").lit("restore"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::text()),
            StructField::new("include", backup_selection_schema()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("restored", TypeSchema::array(TypeSchema::text())),
            StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Automated backups**: any number of recurring backups, each sent to a
    // directory on the server, an SFTP server or an S3-compatible bucket
    // (Settings → Backup → Automated backups). Each has
    // its own `include`, the same selection the backup dialog sends, and the
    // server stores it as what was left out, as it does the Backup card's — so
    // a table created later is in the next run. Read back, `include` is that
    // resolved against what there is now. The `last_*` fields are what the
    // schedule last did, written by the server's backup task and read-only.
    set.register(
        Endpoint::new(
            "listBackupSchedules",
            Method::Get,
            api().lit("backup").lit("schedules"),
        )
        .output(TypeSchema::array(backup_schedule_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createBackupSchedule",
            Method::Post,
            api().lit("backup").lit("schedules"),
        )
        .input(backup_schedule_input_schema())
        .output(backup_schedule_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateBackupSchedule",
            Method::Put,
            api()
                .lit("backup")
                .lit("schedules")
                .param("id", ValueType::Uuid),
        )
        .input(backup_schedule_input_schema())
        .output(backup_schedule_schema())
        .auth(AuthRequirement::admin()),
    );

    // Stops new backups; the ones already written stay where they were sent.
    set.register(
        Endpoint::new(
            "deleteBackupSchedule",
            Method::Delete,
            api()
                .lit("backup")
                .lit("schedules")
                .param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Clear all**: back to an empty installation (Settings → Backup).
    //
    // Two calls because the dialog asks a question first: every file store and
    // the directory it occupies, so the admin can choose which ones leave the
    // disk as well as the database. `delete_from_disk` names stores by name; a
    // store left out keeps its files where they are.
    set.register(
        Endpoint::new("getClearAllPreview", Method::Get, api().lit("clear-all"))
            .output(TypeSchema::struct_of([StructField::new(
                "file_stores",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("name", TypeSchema::text()),
                    StructField::new("backend", TypeSchema::text()),
                    StructField::new("directory", TypeSchema::optional(TypeSchema::text())),
                ])),
            )]))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("clearAll", Method::Post, api().lit("clear-all"))
            .input(TypeSchema::struct_of([StructField::new(
                "delete_from_disk",
                TypeSchema::array(TypeSchema::text()),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("cleared", TypeSchema::array(TypeSchema::text())),
                StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );
}

/// What an admin sets on an automated backup: where the backups go, `daily`,
/// `weekly` or `monthly`, the days before a backup there is deleted, and what each
/// backup includes.
fn backup_schedule_input_schema() -> TypeSchema {
    TypeSchema::struct_of(backup_schedule_input_fields())
}

fn backup_schedule_input_fields() -> Vec<StructField> {
    vec![
        StructField::new("destination", backup_destination_schema()),
        StructField::new("frequency", TypeSchema::text()),
        StructField::new("retention_days", TypeSchema::int()),
        StructField::new("include", backup_selection_schema()),
        // Kinds included whole: every table (definition and rows), application
        // or file store there is when the backup runs, including ones created
        // after the schedule was saved. Absent is all three on; a flag set
        // false leaves that kind to `include`'s lists.
        StructField::new(
            "include_all",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("tables", TypeSchema::bool()),
                StructField::new("applications", TypeSchema::bool()),
                StructField::new("file_stores", TypeSchema::bool()),
            ])),
        ),
        // The password each backup is protected with (WinZip AES-256, which
        // 7-Zip opens); null or empty for none. Sent as the mask when one is
        // stored, and handing the mask back on an update keeps it.
        StructField::new("password", TypeSchema::optional(TypeSchema::text())),
    ]
}

/// Where an automated backup goes. `kind` is `local`, `sftp` or `s3`, and
/// says which of the other fields apply:
///
/// - `local`: `directory`, an absolute path on the server.
/// - `sftp`: `host`, `port` (22 when null), `username`, `password` and
///   `directory` (absolute, or relative to the login's directory; empty for
///   that directory). `host_key` is the server's key fingerprint, recorded when
///   the schedule is saved and checked on every run; it is read-only.
/// - `s3`: `endpoint` (any S3-compatible service; empty for AWS), `bucket`,
///   `region` (empty for `us-east-1`), `access_key` and `secret_key`.
///
/// `password` and `secret_key` are sent as a mask; handing the mask back on an
/// update keeps what is stored.
fn backup_destination_schema() -> TypeSchema {
    let text = || TypeSchema::optional(TypeSchema::text());
    TypeSchema::struct_of([
        StructField::new("kind", TypeSchema::text()),
        StructField::new("directory", text()),
        StructField::new("host", text()),
        StructField::new("port", TypeSchema::optional(TypeSchema::int())),
        StructField::new("username", text()),
        StructField::new("password", text()),
        StructField::new("host_key", text()),
        StructField::new("endpoint", text()),
        StructField::new("bucket", text()),
        StructField::new("region", text()),
        StructField::new("access_key", text()),
        StructField::new("secret_key", text()),
    ])
}

/// An automated backup, with what it last did.
fn backup_schedule_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(backup_schedule_input_fields());
    fields.extend([
        // Where the backups go, in one line: a path, an `sftp://` address or
        // the bucket's URL.
        StructField::new("location", TypeSchema::text()),
        StructField::new(
            "last_attempt_at",
            TypeSchema::optional(TypeSchema::timestamp()),
        ),
        StructField::new(
            "last_success_at",
            TypeSchema::optional(TypeSchema::timestamp()),
        ),
        // Why the last attempt failed; null when it succeeded.
        StructField::new("last_error", TypeSchema::optional(TypeSchema::text())),
        // Where the last backup written is: a path, an `sftp://` address or a
        // URL.
        StructField::new("last_file", TypeSchema::optional(TypeSchema::text())),
    ]);
    TypeSchema::struct_of(fields)
}

/// One thing a backup can include or leave out: what it is called, what to show,
/// and how much of it there is (rows or files; `null` where counting it would cost
/// more than the number is worth).
fn backup_item_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("count", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// Everything that could go into a backup — of this server, or of a backup file
/// that has been uploaded. **One schema for both**, which is what lets one dialog
/// drive the backup and the restore: the difference between them is where the
/// value came from, not what it is.
fn backup_contents_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("tables", TypeSchema::array(backup_item_schema())),
        StructField::new("applications", TypeSchema::array(backup_item_schema())),
        StructField::new("file_stores", TypeSchema::array(backup_item_schema())),
        // Counts rather than flags, because "back up the users" is a different
        // decision when there are two of them and when there are twelve thousand.
        StructField::new("users", TypeSchema::int()),
        StructField::new("modules", TypeSchema::int()),
        StructField::new("db_connections", TypeSchema::int()),
        StructField::new("streams", TypeSchema::int()),
        // The Analytics choice carries all three; they are counted apart so
        // the dialog can say what is in it.
        StructField::new("datasets", TypeSchema::int()),
        StructField::new("models", TypeSchema::int()),
        StructField::new("workspaces", TypeSchema::int()),
        // Fitted model instances, with their draws and output frames: a
        // choice of their own because they can outweigh everything else.
        StructField::new("fits", TypeSchema::int()),
        StructField::new("llm_providers", TypeSchema::int()),
        StructField::new("agents", TypeSchema::int()),
        StructField::new("code_libraries", TypeSchema::int()),
        StructField::new("triggers", TypeSchema::int()),
        // The views and pages of every application on offer. They travel with
        // their application: a view restored without the application it belongs
        // to has nowhere to go.
        StructField::new("views", TypeSchema::int()),
        StructField::new("pages", TypeSchema::int()),
        StructField::new("ssl", TypeSchema::bool()),
        // Every other settings section: email, localisation, development.
        StructField::new("settings", TypeSchema::bool()),
    ])
}

/// What one backup or restore includes.
///
/// `table_data` is separate from `tables` because metadata and rows are separate
/// decisions — a schema-only backup is a real thing to want — and it is a **subset**
/// of it: rows restored into a table nobody described would be unreadable, so the
/// server narrows this to `tables` on the way in rather than trusting it.
fn backup_selection_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        StructField::new("table_data", TypeSchema::array(TypeSchema::text())),
        StructField::new("applications", TypeSchema::array(TypeSchema::text())),
        StructField::new("file_stores", TypeSchema::array(TypeSchema::text())),
        StructField::new("users", TypeSchema::bool()),
        StructField::new("modules", TypeSchema::bool()),
        StructField::new("db_connections", TypeSchema::bool()),
        StructField::new("streams", TypeSchema::bool()),
        StructField::new("analytics", TypeSchema::bool()),
        // Only with `analytics`, which the server enforces as it does rows
        // with their table.
        StructField::new("fits", TypeSchema::bool()),
        StructField::new("llm_providers", TypeSchema::bool()),
        StructField::new("agents", TypeSchema::bool()),
        StructField::new("code_libraries", TypeSchema::bool()),
        StructField::new("triggers", TypeSchema::bool()),
        StructField::new("views", TypeSchema::bool()),
        StructField::new("pages", TypeSchema::bool()),
        StructField::new("ssl", TypeSchema::bool()),
        StructField::new("settings", TypeSchema::bool()),
    ])
}
