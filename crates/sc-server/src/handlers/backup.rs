//! Backup and restore, automated backups and clear all.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // Four handlers for one screen, split by what crosses the wire rather than by
    // what they do (§16): the *choice* is JSON and typed, the *archive* is bytes
    // and is not. `createBackup` and `uploadBackup` are reached by routes outside
    // the endpoint set (see `crate::router`) and are in this registry all the same,
    // so all four are behind the same admin check and the same catalog.
    //
    // The uploaded archive waits in `pending` between the upload and the restore,
    // because what to restore cannot be chosen until the file has been read.

    let pending = sc_backup::PendingUploads::new();

    // What this server has to offer, plus the selection the admin last made — so
    // the dialog opens on their tuned choice rather than on everything, and on
    // *everything* the first time.
    reg.handle(sc_api::handler_ids::GET_BACKUP_OPTIONS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let available = sc_backup::available(&catalog).await?;
                let preferences = stored_backup_preferences(&catalog).await?;
                Ok(HandlerResponse::ok(json!({
                    "available": available.to_json(),
                    "include": preferences.selection(&available).to_json(),
                })))
            }
        }
    });

    // Build the archive, and remember what was asked for.
    //
    // The selection is persisted **here**, on the way to producing a file, rather
    // than by a save button of its own: the admin's answer to "what should a backup
    // include" is exactly the backup they just took, and a screen that made them
    // state it twice would drift.
    reg.handle(sc_api::handler_ids::CREATE_BACKUP, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let available = sc_backup::available(&catalog).await?;
                let selection = match ctx.body.get("include") {
                    Some(value) => sc_backup::Selection::from_json(value)?,
                    // No selection at all means everything, which is the default
                    // the screen starts from and what a script with no opinion
                    // should get.
                    None => sc_backup::Selection::everything(&available),
                };
                // Checked before the selection is remembered, so a refused
                // password changes nothing. It is not remembered itself: the
                // next backup is protected only if the admin says so again.
                let password = sc_backup::password_from_body(&ctx.body)?;
                let previous = stored_backup_preferences(&catalog).await?;
                let preferences =
                    sc_backup::BackupPreferences::of(&previous, &available, &selection);
                sc_config::set_config(&catalog, sc_config::BACKUP_INCLUDE, preferences.to_json())
                    .await?;

                // Written to the spool directory and served from there, so the
                // archive is never in memory; an error before the first byte is
                // still the JSON error every other handler gives.
                let spool = sc_backup::SpoolFile::new("backup");
                let len = sc_backup::write_backup_to(
                    &catalog,
                    &selection,
                    password.as_deref(),
                    spool.create()?,
                )
                .await?;
                Ok(HandlerResponse::download(crate::handler::Download {
                    body: crate::handler::DownloadBody::File(crate::handler::SpooledFile::open(
                        spool, len,
                    )?),
                    content_type: "application/zip".to_owned(),
                    filename: Some(sc_http::DownloadName::sanitised(&backup_filename())),
                }))
            }
        }
    });

    // Take delivery of a file and say what is in it. Nothing is written.
    //
    // A password-protected file cannot say what is in it yet: it is kept all
    // the same and answered with `locked` and its `id`, and the dialog asks
    // for the password and hands it to `unlockBackup`, which answers what this
    // would have.
    reg.handle(sc_api::handler_ids::UPLOAD_BACKUP, {
        let pending = pending.clone();
        move |ctx| {
            let pending = pending.clone();
            async move {
                let file = ctx
                    .raw_file
                    .clone()
                    .ok_or_else(|| Error::invalid("this handler requires an uploaded file"))?;
                if sc_backup::is_encrypted(file.path())? {
                    let id = pending.keep(file)?;
                    return Ok(HandlerResponse::ok(json!({
                        "id": id.to_string(),
                        "encrypted": true,
                        "locked": true,
                    })));
                }
                let (contents, manifest) = sc_backup::inspect(file.path(), None)?;
                let id = pending.keep(file)?;
                Ok(HandlerResponse::ok(uploaded_backup_json(
                    id, false, &contents, &manifest,
                )))
            }
        }
    });

    // The password of an uploaded, protected backup: checked against the file,
    // held with it for the restore, and answered with what the file holds. A
    // wrong password is refused and the file stays, so the admin can try again.
    reg.handle(sc_api::handler_ids::UNLOCK_BACKUP, {
        let pending = pending.clone();
        move |ctx| {
            let pending = pending.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let id = parse_uuid(obj.non_empty_str_field("id")?, "uploaded backup")?;
                let password = sc_backup::password_from_body(&ctx.body)?
                    .ok_or_else(|| Error::invalid("`password`: enter the backup's password"))?;
                let file = pending.file(id)?;
                let (contents, manifest) = {
                    let path = file.path().to_owned();
                    let password = password.clone();
                    // Decrypting the manifest derives a key (PBKDF2, 1000
                    // rounds) and reads an entry: off the async threads.
                    tokio::task::spawn_blocking(move || sc_backup::inspect(&path, Some(&password)))
                        .await
                        .map_err(|e| Error::msg(format!("reading the backup: {e}")))??
                };
                pending.unlock(id, password)?;
                Ok(HandlerResponse::ok(uploaded_backup_json(
                    id, true, &contents, &manifest,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RESTORE_BACKUP, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        let pending = pending.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            let pending = pending.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let id = parse_uuid(obj.non_empty_str_field("id")?, "uploaded backup")?;
                let selection = match obj.get("include") {
                    Some(value) => sc_backup::Selection::from_json(value)?,
                    None => {
                        return Err(Error::invalid(
                            "`include` must say what to restore from the backup",
                        ));
                    }
                };
                // Deleted when this is done with it, whichever way the restore
                // goes.
                // Refused, not taken: the file stays for `unlockBackup`.
                if pending.is_locked(id)? {
                    return Err(Error::invalid(
                        "this backup is protected by a password: unlock it with its password \
                         before restoring",
                    ));
                }
                // The password `unlockBackup` checked, when the file has one.
                let (file, password) = pending.take(id)?;
                let report = sc_backup::restore_backup(
                    &catalog,
                    apps.as_ref(),
                    file.path(),
                    password.as_deref(),
                    &selection,
                    ctx.who.editor(),
                )
                .await?;
                // A restore can create tables and stores an application's API is
                // projected from, so the mounted apps are re-projected once at the
                // end — the same thing a schema change through the admin API does.
                reproject_apps(&apps);
                Ok(HandlerResponse::ok(json!({
                    "restored": report.restored,
                    "warnings": report.warnings,
                })))
            }
        }
    });

    // The schedules are a list in `_fd_config`; what each last did is a second
    // key only the backup scheduler writes (`sc_backup::schedule`), merged in
    // here so the card can say when each last ran and whether it failed.
    //
    // A destination's secrets (an SFTP password, an S3 secret key) go out as
    // the mask and come back as it when unchanged (`keep_secrets`). Saving
    // reaches the destination (`check`), which for SFTP is also when the
    // server's host key is learnt and recorded.

    reg.handle(sc_api::handler_ids::LIST_BACKUP_SCHEDULES, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                use sc_backup::schedule::{load_schedules, load_statuses, schedule_json};
                let schedules = load_schedules(&catalog).await?;
                let statuses = load_statuses(&catalog).await?;
                let available = sc_backup::available(&catalog).await?;
                let list: Vec<Json> = schedules
                    .iter()
                    .map(|s| schedule_json(s, statuses.get(&s.id), &available))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(list)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_BACKUP_SCHEDULE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                use sc_backup::schedule::{
                    BackupSchedule, check_unique, include_from_body, load_schedules,
                    save_schedules, schedule_json,
                };
                let body = require_object(&ctx.body)?;
                let available = sc_backup::available(&catalog).await?;
                let include = include_from_body(body, &available, None)?;
                let mut schedule = BackupSchedule::from_body(uuid::Uuid::new_v4(), body, include)?
                    .keep_password(None)?;
                schedule.destination = schedule.destination.keep_secrets(None)?;
                let mut schedules = load_schedules(&catalog).await?;
                check_unique(&schedule, &schedules)?;
                schedule.destination.check().await?;
                schedules.push(schedule.clone());
                save_schedules(&catalog, &schedules).await?;
                Ok(HandlerResponse::ok(schedule_json(
                    &schedule, None, &available,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_BACKUP_SCHEDULE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                use sc_backup::schedule::{
                    BackupSchedule, check_unique, include_from_body, load_schedules, load_statuses,
                    save_schedules, schedule_json,
                };
                let id = parse_uuid(ctx.path_param("id")?, "backup schedule")?;
                let body = require_object(&ctx.body)?;
                let available = sc_backup::available(&catalog).await?;
                let mut schedules = load_schedules(&catalog).await?;
                let Some(index) = schedules.iter().position(|s| s.id == id) else {
                    return Err(Error::not_found(format!(
                        "no automated backup with id {id}"
                    )));
                };
                let previous = &schedules[index];
                // The previous exclusions are an input, so one naming a table
                // that is not here today survives the edit.
                let include = include_from_body(body, &available, Some(&previous.include))?;
                let mut schedule =
                    BackupSchedule::from_body(id, body, include)?.keep_password(Some(previous))?;
                schedule.destination = schedule
                    .destination
                    .keep_secrets(Some(&previous.destination))?;
                check_unique(&schedule, &schedules)?;
                schedule.destination.check().await?;
                schedules[index] = schedule.clone();
                save_schedules(&catalog, &schedules).await?;
                let statuses = load_statuses(&catalog).await?;
                Ok(HandlerResponse::ok(schedule_json(
                    &schedule,
                    statuses.get(&id),
                    &available,
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_BACKUP_SCHEDULE, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                use sc_backup::schedule::{load_schedules, save_schedules};
                let id = parse_uuid(ctx.path_param("id")?, "backup schedule")?;
                let mut schedules = load_schedules(&catalog).await?;
                let before = schedules.len();
                schedules.retain(|s| s.id != id);
                let deleted = schedules.len() != before;
                if deleted {
                    save_schedules(&catalog, &schedules).await?;
                }
                // The backups already written stay where they are: deleting a
                // schedule stops new ones, it does not take the old ones with it.
                // (Its stored credentials do go, with the schedule.)
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_CLEAR_ALL_PREVIEW, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                Ok(HandlerResponse::ok(
                    crate::clear_all::preview(&catalog).await?,
                ))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CLEAR_ALL, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let delete_from_disk: Vec<String> = match obj.get("delete_from_disk") {
                    Some(Json::Array(names)) => names
                        .iter()
                        .filter_map(Json::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => {
                        return Err(Error::invalid(
                            "`delete_from_disk` must list the file stores to remove from disk",
                        ));
                    }
                };
                let report = crate::clear_all::clear_all(
                    &catalog,
                    &apps,
                    &delete_from_disk,
                    ctx.who.editor(),
                )
                .await?;
                // Every account is gone, the caller's included, so every session
                // goes too: the next status says no user exists and the admin UI
                // shows the create-first-user screen.
                Ok(HandlerResponse::end_all_sessions(json!({
                    "cleared": report.cleared,
                    "warnings": report.warnings,
                })))
            }
        }
    });
}

/// The selection an admin last made, or "everything" when they never have.
async fn stored_backup_preferences(catalog: &Catalog) -> Result<sc_backup::BackupPreferences> {
    let stored = sc_config::stored_config(catalog, sc_config::BACKUP_INCLUDE).await?;
    Ok(match stored {
        Some(value) => sc_backup::BackupPreferences::from_json(&value),
        None => sc_backup::BackupPreferences::default(),
    })
}

/// What `uploadBackup` (and `unlockBackup`, for a protected file) answers: the
/// token that names the held file and what the file holds. `encrypted` says
/// whether the file is protected by a password; `locked` is false, because
/// what it holds has been read.
fn uploaded_backup_json(
    id: uuid::Uuid,
    encrypted: bool,
    contents: &sc_backup::Available,
    manifest: &Json,
) -> Json {
    json!({
        "id": id.to_string(),
        "encrypted": encrypted,
        "locked": false,
        "created_at": manifest.get("created_at"),
        // What wrote the file, in one line — "Feldspar 0.1.0", or "Saltcorn
        // 1.7.0, imported" for a v1 archive this server translated on the way
        // in. The restore dialog shows it, because a v1 import leaves things
        // behind and the admin should know which kind of file they picked
        // before they press the button.
        "source": manifest.get("source"),
        "available": contents.to_json(),
        // Everything the file holds, ticked: the admin excludes from there,
        // which is the same direction the backup dialog works in.
        "include": sc_backup::Selection::everything(contents).to_json(),
    })
}

/// What a downloaded backup is called: the date and time it was taken, to the
/// second, so a directory of them sorts chronologically and two taken the same
/// afternoon do not overwrite each other.
///
/// The same name an automated backup is written under, so a directory can hold
/// both and the automated backups' pruning treats them alike.
fn backup_filename() -> String {
    sc_backup::schedule::backup_file_name(chrono::Utc::now())
}
