//! Automated backups: any number of recurring backups, each sent to a
//! directory on the server, an SFTP server or an S3-compatible bucket, and
//! pruned after a number of days.
//!
//! A schedule is four things an admin chooses — a **destination** (a
//! [`Destination`]: a local directory, an SFTP directory or a bucket), a
//! **frequency** (daily, weekly or monthly), a **retention** (days before a
//! backup there is deleted) and what it **includes** — stored as a list under
//! [`BACKUP_SCHEDULES`](sc_config::BACKUP_SCHEDULES). Each schedule has its own
//! selection, so a nightly schema-only backup and a weekly full one can sit
//! side by side.
//!
//! The selection is stored the way the Backup card's is: as
//! [`BackupPreferences`], what was *left out*. A table added next week is in
//! next week's automated backup unless somebody unticks it, for the reason the
//! manual backup works that way — a schedule that quietly stopped covering new
//! tables is the worst way to find out how it works.
//!
//! [`BackupScheduler`] runs them: one task, waking on the minute like the trigger
//! scheduler, running the due schedules **one after another** — two full
//! backups at once would compete for the same database for no benefit, and
//! serialising them is also what keeps the status record
//! ([`BACKUP_SCHEDULE_STATUS`](sc_config::BACKUP_SCHEDULE_STATUS)) free of
//! lost updates, since this task is its only writer.
//!
//! The rules, each a pure function tested against instants rather than waited
//! for:
//!
//! - **Due** ([`is_due`]): a schedule that has never succeeded is due at once —
//!   the admin learns on the next minute whether the backup arrives, not a
//!   week later. After a success it is due a day, a week or a calendar month
//!   after the tick that ran it; after a failure it is retried an hour later rather than every
//!   minute.
//! - **Written whole or not at all** ([`Connection::put`]): on a disk or over
//!   SFTP the zip is written under a dot-prefixed temporary name and renamed
//!   into place, so neither a reader nor the pruning below ever sees half a
//!   backup; an S3 put is atomic already.
//! - **Pruned by name** ([`expired`]): only files named as a backup is named
//!   (`feldspar-backup-YYYY-MM-DD-HHMMSS.zip`) are ever deleted, and their age is
//!   the time in that name, not the file's mtime — a copy or a restore of the
//!   directory must not make every backup in it look new, or old. The same rule
//!   for every kind of destination, since each only lists and deletes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Months, NaiveDateTime, Timelike, Utc};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use super::destination::{Connection, Destination};
use super::{Available, BackupPreferences};

/// The prefix and suffix of a backup's file name, with the time it was taken
/// between them (see [`backup_file_name`]).
const FILE_PREFIX: &str = "feldspar-backup-";
const FILE_SUFFIX: &str = ".zip";
/// The time format between them: sorts chronologically as text.
const FILE_TIME: &str = "%Y-%m-%d-%H%M%S";

/// How long a failed run waits before it is tried again.
const RETRY_AFTER: Duration = Duration::hours(1);

/// The longest retention accepted: ten years, which is "keep them" in every
/// sense that matters and stops a typo of extra digits from being stored.
pub const MAX_RETENTION_DAYS: i64 = 3650;

/// How often a schedule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
}

impl Frequency {
    pub fn as_str(self) -> &'static str {
        match self {
            Frequency::Daily => "daily",
            Frequency::Weekly => "weekly",
            Frequency::Monthly => "monthly",
        }
    }

    pub fn parse(raw: &str) -> Result<Frequency> {
        match raw {
            "daily" => Ok(Frequency::Daily),
            "weekly" => Ok(Frequency::Weekly),
            "monthly" => Ok(Frequency::Monthly),
            other => Err(Error::invalid(format!(
                "`{other}` is not a backup frequency; choose `daily`, `weekly` or `monthly`"
            ))),
        }
    }

    /// When the run after one at `last` is due. A month is a calendar month,
    /// not thirty days, so a backup taken on the 3rd is next taken on the 3rd;
    /// from a day the next month lacks it is that month's last day (31 January
    /// is followed by 28 or 29 February, and that by the 28th or 29th of March).
    pub fn next_after(self, last: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Frequency::Daily => last + Duration::days(1),
            Frequency::Weekly => last + Duration::weeks(1),
            Frequency::Monthly => last
                .checked_add_months(Months::new(1))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        }
    }
}

/// One automated backup.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupSchedule {
    pub id: Uuid,
    pub destination: Destination,
    pub frequency: Frequency,
    /// Backups at `destination` older than this many days are deleted after
    /// each successful run.
    pub retention_days: i64,
    /// What each backup includes, as what is left out.
    pub include: BackupPreferences,
}

impl BackupSchedule {
    /// Read a schedule from an admin's request body (`destination`,
    /// `frequency`, `retention_days`), with the id it is to have and what it
    /// includes (which the caller works out from the body's `include`
    /// selection; see [`include_from_body`]).
    ///
    /// Checks what can be checked without reaching the destination or reading
    /// the other schedules; see [`Destination::check`] and [`check_unique`]
    /// for those. A secret left as the mask is still the mask here; see
    /// [`Destination::keep_secrets`].
    pub fn from_body(
        id: Uuid,
        body: &Map<String, Json>,
        include: BackupPreferences,
    ) -> Result<BackupSchedule> {
        let destination = Destination::from_body(body.get("destination"))?;
        BackupSchedule::with_destination(id, body, destination, include)
    }

    fn with_destination(
        id: Uuid,
        body: &Map<String, Json>,
        destination: Destination,
        include: BackupPreferences,
    ) -> Result<BackupSchedule> {
        let frequency = Frequency::parse(
            body.get("frequency")
                .and_then(Json::as_str)
                .unwrap_or_default(),
        )?;
        let retention_days = body
            .get("retention_days")
            .and_then(Json::as_i64)
            .ok_or_else(|| Error::invalid("`retention_days` must be a whole number of days"))?;
        if !(1..=MAX_RETENTION_DAYS).contains(&retention_days) {
            return Err(Error::invalid(format!(
                "`retention_days` must be between 1 and {MAX_RETENTION_DAYS}"
            )));
        }
        Ok(BackupSchedule {
            id,
            destination,
            frequency,
            retention_days,
            include,
        })
    }

    /// The stored shape.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id.to_string(),
            "destination": self.destination.to_json(),
            "frequency": self.frequency.as_str(),
            "retention_days": self.retention_days,
            "include": self.include.to_json(),
        })
    }

    /// Read a stored schedule. A record that no longer parses is skipped by
    /// [`load_schedules`] rather than failing the list, so one bad entry cannot
    /// stop every other schedule from running.
    fn from_json(value: &Json) -> Option<BackupSchedule> {
        let obj = value.as_object()?;
        let id = Uuid::parse_str(obj.get("id")?.as_str()?).ok()?;
        let include = obj
            .get("include")
            .map(BackupPreferences::from_json)
            .unwrap_or_default();
        let destination = Destination::from_stored(obj.get("destination")?)?;
        BackupSchedule::with_destination(id, obj, destination, include).ok()
    }
}

/// What a schedule last did. Written only by [`BackupScheduler`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScheduleStatus {
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    /// Why the last attempt failed; `None` when it succeeded.
    pub last_error: Option<String>,
    /// Where the last backup written is: a path, an `sftp://` address or a URL.
    pub last_file: Option<String>,
}

impl ScheduleStatus {
    pub fn to_json(&self) -> Json {
        json!({
            "last_attempt_at": self.last_attempt_at.map(|t| t.to_rfc3339()),
            "last_success_at": self.last_success_at.map(|t| t.to_rfc3339()),
            "last_error": self.last_error,
            "last_file": self.last_file,
        })
    }

    fn from_json(value: &Json) -> ScheduleStatus {
        let time = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
        };
        let text = |key: &str| value.get(key).and_then(Json::as_str).map(str::to_owned);
        ScheduleStatus {
            last_attempt_at: time("last_attempt_at"),
            last_success_at: time("last_success_at"),
            last_error: text("last_error"),
            last_file: text("last_file"),
        }
    }
}

/// Refuse a second schedule writing to the same place (directory or bucket):
/// each one's pruning would delete the other's backups.
pub fn check_unique(schedule: &BackupSchedule, others: &[BackupSchedule]) -> Result<()> {
    let place = schedule.destination.place();
    if others
        .iter()
        .any(|o| o.id != schedule.id && o.destination.place() == place)
    {
        return Err(Error::invalid(schedule.destination.place_taken()));
    }
    Ok(())
}

/// The stored schedules, in the order they were created.
pub async fn load_schedules(catalog: &Catalog) -> Result<Vec<BackupSchedule>> {
    let stored = sc_config::stored_config(catalog, sc_config::BACKUP_SCHEDULES).await?;
    Ok(stored
        .as_ref()
        .and_then(Json::as_array)
        .map(|list| list.iter().filter_map(BackupSchedule::from_json).collect())
        .unwrap_or_default())
}

/// Replace the stored schedules.
pub async fn save_schedules(catalog: &Catalog, schedules: &[BackupSchedule]) -> Result<()> {
    let list: Vec<Json> = schedules.iter().map(BackupSchedule::to_json).collect();
    sc_config::set_config(catalog, sc_config::BACKUP_SCHEDULES, Json::Array(list)).await
}

/// What each schedule last did, by id.
pub async fn load_statuses(catalog: &Catalog) -> Result<HashMap<Uuid, ScheduleStatus>> {
    let stored = sc_config::stored_config(catalog, sc_config::BACKUP_SCHEDULE_STATUS).await?;
    Ok(stored
        .as_ref()
        .and_then(Json::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(id, status)| {
                    Some((Uuid::parse_str(id).ok()?, ScheduleStatus::from_json(status)))
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn save_statuses(catalog: &Catalog, statuses: &HashMap<Uuid, ScheduleStatus>) -> Result<()> {
    let map: Map<String, Json> = statuses
        .iter()
        .map(|(id, status)| (id.to_string(), status.to_json()))
        .collect();
    sc_config::set_config(
        catalog,
        sc_config::BACKUP_SCHEDULE_STATUS,
        Json::Object(map),
    )
    .await
}

/// What a request body's `include` selection means for a schedule, given what
/// there is to choose from now and what the schedule left out before (`None`
/// for a new one).
///
/// The same arithmetic the manual backup does with the Backup card's
/// selection ([`BackupPreferences::of`]): a name not on offer keeps whatever it
/// had, so an exclusion is not erased by editing the schedule on a day the table
/// happens not to exist.
pub fn include_from_body(
    body: &Map<String, Json>,
    available: &Available,
    previous: Option<&BackupPreferences>,
) -> Result<BackupPreferences> {
    let selection = match body.get("include") {
        Some(value) => super::Selection::from_json(value)?,
        None => {
            return Err(Error::invalid(
                "`include` must say what each automated backup includes",
            ));
        }
    };
    let previous = previous.cloned().unwrap_or_default();
    Ok(BackupPreferences::of(&previous, available, &selection))
}

/// A schedule with what it last did, as `listBackupSchedules` returns it: its
/// `include` is the selection it means for what is on offer *now* — the shape
/// the dialog's pickers take — its destination's secrets are masked, and
/// `location` says where the backups go in one line.
pub fn schedule_json(
    schedule: &BackupSchedule,
    status: Option<&ScheduleStatus>,
    available: &Available,
) -> Json {
    let mut out = schedule.to_json();
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "include".to_owned(),
            schedule.include.selection(available).to_json(),
        );
        obj.insert(
            "destination".to_owned(),
            schedule.destination.redacted_json(),
        );
        obj.insert(
            "location".to_owned(),
            Json::String(schedule.destination.location()),
        );
    }
    let status = status.cloned().unwrap_or_default().to_json();
    if let (Some(out), Some(status)) = (out.as_object_mut(), status.as_object()) {
        out.extend(status.clone());
    }
    out
}

/// Whether `schedule` should run at `now`, given what it last did.
pub fn is_due(
    schedule: &BackupSchedule,
    status: Option<&ScheduleStatus>,
    now: DateTime<Utc>,
) -> bool {
    let Some(status) = status else {
        return true;
    };
    if status.last_error.is_some()
        && let Some(attempt) = status.last_attempt_at
        && now < attempt + RETRY_AFTER
    {
        return false;
    }
    match status.last_success_at {
        None => true,
        Some(success) => now >= schedule.frequency.next_after(success),
    }
}

/// What a backup taken at `at` is called — the name a manual download gets too,
/// so a directory can hold both and [`prune`] treats them alike.
pub fn backup_file_name(at: DateTime<Utc>) -> String {
    format!("{FILE_PREFIX}{}{FILE_SUFFIX}", at.format(FILE_TIME))
}

/// When the backup called `name` was taken, or `None` for any other file.
fn taken_at(name: &str) -> Option<DateTime<Utc>> {
    let stamp = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDateTime::parse_from_str(stamp, FILE_TIME)
        .ok()
        .map(|t| t.and_utc())
}

/// Which of `names` are backups taken more than `retention_days` before
/// `now`. Anything not named as a backup is never among them.
pub fn expired(names: &[String], retention_days: i64, now: DateTime<Utc>) -> Vec<String> {
    let cutoff = now - Duration::days(retention_days);
    names
        .iter()
        .filter(|name| taken_at(name).is_some_and(|taken| taken < cutoff))
        .cloned()
        .collect()
}

/// Delete the expired backups at a destination, returning the names deleted.
pub async fn prune(
    conn: &Connection,
    retention_days: i64,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let names = conn.list().await?;
    let doomed = expired(&names, retention_days, now);
    for name in &doomed {
        conn.delete(name).await?;
    }
    Ok(doomed)
}

/// Take one backup for `schedule`, named for `at`, send it to the
/// destination and prune there. Returns where the backup went.
pub async fn run_schedule(
    catalog: &Catalog,
    schedule: &BackupSchedule,
    at: DateTime<Utc>,
) -> Result<String> {
    // The backup first: no point holding a connection open while it is built.
    let available: Available = super::available(catalog).await?;
    let bytes = super::write_backup(catalog, &schedule.include.selection(&available)).await?;

    let conn = schedule.destination.open().await?;
    let result = async {
        let written = conn.put(&backup_file_name(at), bytes).await?;
        prune(&conn, schedule.retention_days, at).await?;
        Ok(written)
    }
    .await;
    conn.close().await;
    result
}

/// The task that runs the automated backups.
pub struct BackupScheduler {
    catalog: Arc<Catalog>,
    /// Set while a tick's backups are running, so a run that takes longer than
    /// a minute is not joined by a second copy of itself.
    busy: Mutex<bool>,
}

impl BackupScheduler {
    pub fn new(catalog: Arc<Catalog>) -> BackupScheduler {
        BackupScheduler {
            catalog,
            busy: Mutex::new(false),
        }
    }

    /// Start the task loop: a tick on every minute boundary.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let now = Utc::now();
                let wait = 60 - u64::from(now.second());
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                let scheduler = Arc::clone(&scheduler);
                // A task of its own, so a backup that takes ten minutes does
                // not stop the clock; `busy` keeps the next ticks from starting
                // another until it is done.
                tokio::spawn(async move { scheduler.tick(Utc::now()).await });
            }
        })
    }

    /// Run every schedule that is due at `now`, one after another, recording
    /// what each did. Returns the ids that ran (for tests); a tick that finds a
    /// previous one still running does nothing.
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<Uuid> {
        {
            let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
            if *busy {
                return Vec::new();
            }
            *busy = true;
        }
        let ran = self.run_due(now).await;
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
        ran
    }

    async fn run_due(&self, now: DateTime<Utc>) -> Vec<Uuid> {
        // On the minute, so a daily backup recorded at 02:00 is due at 02:00
        // tomorrow and does not creep later by the seconds a tick wakes late.
        let now = now
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now);
        let (schedules, mut statuses) = match (
            load_schedules(&self.catalog).await,
            load_statuses(&self.catalog).await,
        ) {
            (Ok(s), Ok(st)) => (s, st),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("feldspar: the backup scheduler could not read its schedules: {e}");
                return Vec::new();
            }
        };
        let due: Vec<&BackupSchedule> = schedules
            .iter()
            .filter(|s| is_due(s, statuses.get(&s.id), now))
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        let mut ran = Vec::new();
        for schedule in due {
            let status = statuses.entry(schedule.id).or_default();
            status.last_attempt_at = Some(now);
            match run_schedule(&self.catalog, schedule, now).await {
                Ok(written) => {
                    status.last_success_at = Some(now);
                    status.last_error = None;
                    status.last_file = Some(written);
                }
                Err(e) => {
                    eprintln!(
                        "feldspar: automated backup to `{}` failed: {e}",
                        schedule.destination.location()
                    );
                    status.last_error = Some(e.to_string());
                }
            }
            ran.push(schedule.id);
        }
        // Forget the status of schedules that have since been deleted. Done
        // here, by the status's one writer, rather than by the delete handler.
        let live: HashSet<Uuid> = schedules.iter().map(|s| s.id).collect();
        statuses.retain(|id, _| live.contains(id));
        if let Err(e) = save_statuses(&self.catalog, &statuses).await {
            eprintln!("feldspar: the backup scheduler could not record what it did: {e}");
        }
        ran
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn schedule(frequency: Frequency) -> BackupSchedule {
        BackupSchedule {
            id: Uuid::new_v4(),
            destination: Destination::Local {
                directory: "/srv/backups".into(),
            },
            frequency,
            retention_days: 7,
            include: BackupPreferences::default(),
        }
    }

    #[test]
    fn a_schedule_that_never_ran_is_due_at_once() {
        assert!(is_due(
            &schedule(Frequency::Weekly),
            None,
            at(2026, 1, 1, 0, 0)
        ));
    }

    #[test]
    fn a_success_makes_the_next_run_one_period_later() {
        let s = schedule(Frequency::Daily);
        let status = ScheduleStatus {
            last_attempt_at: Some(at(2026, 1, 1, 2, 0)),
            last_success_at: Some(at(2026, 1, 1, 2, 0)),
            ..Default::default()
        };
        assert!(!is_due(&s, Some(&status), at(2026, 1, 2, 1, 59)));
        assert!(is_due(&s, Some(&status), at(2026, 1, 2, 2, 0)));

        let w = BackupSchedule {
            frequency: Frequency::Weekly,
            ..s
        };
        assert!(!is_due(&w, Some(&status), at(2026, 1, 7, 2, 0)));
        assert!(is_due(&w, Some(&status), at(2026, 1, 8, 2, 0)));

        let m = BackupSchedule {
            frequency: Frequency::Monthly,
            ..w
        };
        assert!(!is_due(&m, Some(&status), at(2026, 1, 31, 2, 0)));
        assert!(!is_due(&m, Some(&status), at(2026, 2, 1, 1, 59)));
        assert!(is_due(&m, Some(&status), at(2026, 2, 1, 2, 0)));
    }

    #[test]
    fn a_month_is_a_calendar_month() {
        let m = Frequency::Monthly;
        // February is shorter than thirty days, and March is longer.
        assert_eq!(m.next_after(at(2026, 2, 3, 2, 0)), at(2026, 3, 3, 2, 0));
        assert_eq!(m.next_after(at(2026, 3, 3, 2, 0)), at(2026, 4, 3, 2, 0));
        // A day the next month lacks becomes its last day.
        assert_eq!(m.next_after(at(2026, 1, 31, 2, 0)), at(2026, 2, 28, 2, 0));
        assert_eq!(m.next_after(at(2028, 1, 31, 2, 0)), at(2028, 2, 29, 2, 0));
        // Across the year.
        assert_eq!(m.next_after(at(2026, 12, 15, 2, 0)), at(2027, 1, 15, 2, 0));
    }

    #[test]
    fn a_failure_is_retried_an_hour_later_not_every_minute() {
        let s = schedule(Frequency::Weekly);
        let status = ScheduleStatus {
            last_attempt_at: Some(at(2026, 1, 1, 2, 0)),
            last_success_at: None,
            last_error: Some("disk full".into()),
            last_file: None,
        };
        assert!(!is_due(&s, Some(&status), at(2026, 1, 1, 2, 1)));
        assert!(is_due(&s, Some(&status), at(2026, 1, 1, 3, 0)));
    }

    #[test]
    fn a_body_is_checked() {
        let body = |v: Json| v.as_object().unwrap().clone();
        let ok = BackupSchedule::from_body(
            Uuid::nil(),
            &body(json!({
                "destination": { "kind": "local", "directory": "/srv/b" },
                "frequency": "weekly",
                "retention_days": 30
            })),
            BackupPreferences::default(),
        )
        .unwrap();
        assert_eq!(ok.frequency, Frequency::Weekly);
        assert_eq!(ok.retention_days, 30);
        let local = json!({ "kind": "local", "directory": "/srv/b" });
        for bad in [
            json!({ "destination": local, "frequency": "hourly", "retention_days": 30 }),
            json!({ "destination": local, "frequency": "daily", "retention_days": 0 }),
            json!({ "destination": local, "frequency": "daily" }),
            json!({ "destination": "/srv/b", "frequency": "daily", "retention_days": 3 }),
            json!({ "destination": { "kind": "local", "directory": "rel" }, "frequency": "daily", "retention_days": 3 }),
        ] {
            assert!(
                BackupSchedule::from_body(Uuid::nil(), &body(bad), BackupPreferences::default())
                    .is_err()
            );
        }
    }

    #[test]
    fn a_stored_schedule_keeps_what_it_left_out() {
        let s = BackupSchedule {
            include: BackupPreferences {
                exclude_table_data: vec!["books".into()],
                users: false,
                ..BackupPreferences::default()
            },
            ..schedule(Frequency::Daily)
        };
        assert_eq!(BackupSchedule::from_json(&s.to_json()), Some(s));
    }

    #[test]
    fn two_schedules_cannot_share_a_directory() {
        let a = schedule(Frequency::Daily);
        let b = schedule(Frequency::Weekly);
        assert!(check_unique(&b, std::slice::from_ref(&a)).is_err());
        // Editing a schedule is not a clash with itself.
        assert!(check_unique(&a, std::slice::from_ref(&a)).is_ok());
        // A bucket is a place too, whoever's keys write to it.
        let bucket = |key: &str| {
            Destination::from_body(Some(&json!({
                "kind": "s3", "endpoint": "https://s3.example.com", "bucket": "site",
                "access_key": key, "secret_key": "s"
            })))
            .unwrap()
        };
        let c = BackupSchedule {
            destination: bucket("one"),
            ..schedule(Frequency::Daily)
        };
        let d = BackupSchedule {
            destination: bucket("two"),
            ..schedule(Frequency::Daily)
        };
        let err = check_unique(&d, &[a, c]).unwrap_err().to_string();
        assert!(err.contains("bucket"), "{err}");
    }

    #[test]
    fn a_file_name_round_trips_to_its_time() {
        let t = Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap();
        assert_eq!(backup_file_name(t), "feldspar-backup-2026-03-04-050607.zip");
        assert_eq!(taken_at(&backup_file_name(t)), Some(t));
        assert_eq!(taken_at("notes.txt"), None);
        assert_eq!(
            taken_at(".feldspar-backup-2026-03-04-050607.zip.partial"),
            None
        );
    }

    #[tokio::test]
    async fn pruning_deletes_only_old_backups() {
        let dir = std::env::temp_dir().join(format!("sc-prune-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let now = at(2026, 1, 31, 2, 0);
        let old = backup_file_name(at(2026, 1, 20, 2, 0));
        let recent = backup_file_name(at(2026, 1, 28, 2, 0));
        for name in [
            old.as_str(),
            recent.as_str(),
            "notes.txt",
            "feldspar-backup-junk.zip",
        ] {
            tokio::fs::write(dir.join(name), b"x").await.unwrap();
        }
        let conn = Connection::Local(dir.clone());
        let deleted = prune(&conn, 7, now).await.unwrap();
        assert_eq!(deleted, vec![old.clone()]);
        for kept in [recent.as_str(), "notes.txt", "feldspar-backup-junk.zip"] {
            assert!(dir.join(kept).exists(), "{kept} should have been kept");
        }
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
