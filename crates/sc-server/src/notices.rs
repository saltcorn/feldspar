//! Notices for the administrators, and the row-level-security upgrade report
//! (TODO-type-hardening H8.6, TECHNICAL_DESIGN §7.3a "Compatibility").
//!
//! A notice is something the server tells the administrators once: a title,
//! its lines, when. They are kept in one configuration value
//! ([`sc_config::ADMIN_NOTICES`]), listed by `listAdminNotices` and shown by the
//! admin UI until an administrator dismisses them.
//!
//! The first one is the upgrade report. Making RLS a hard boundary changes
//! behaviour on an instance that combines RLS tables with code that elevates: a
//! code body's `asAdmin()` over an RLS table is now an error, and a trigger's
//! default admin authority over one now sees what its cause sees. There is no
//! setting to restore the old behaviour, so on the first start of this version
//! the server lists, in a notice and in the log, every place that is affected —
//! found statically: each code body (a trigger's, a custom query's, a code
//! library's) that says `asAdmin()` and names an RLS table, and each trigger a
//! user's event fires whose table has RLS or whose code names one.

use std::sync::Arc;

use chrono::Utc;
use sc_action::{EventKind, Trigger, TriggerDispatcher};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One notice for the administrators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminNotice {
    /// Stable identity, what dismissing names.
    pub id: String,
    /// One line saying what it is about.
    pub title: String,
    /// What there is to read, one item per line.
    pub lines: Vec<String>,
    /// When the server wrote it, RFC 3339.
    pub created_at: String,
}

/// Every notice not yet dismissed, oldest first.
pub async fn list_notices(catalog: &Catalog) -> Result<Vec<AdminNotice>> {
    match sc_config::config_value(catalog, sc_config::ADMIN_NOTICES).await? {
        Json::Null => Ok(Vec::new()),
        value => serde_json::from_value(value)
            .map_err(|e| Error::invalid(format!("the stored admin notices do not read: {e}"))),
    }
}

/// Add a notice.
pub async fn add_notice(catalog: &Catalog, title: &str, lines: Vec<String>) -> Result<AdminNotice> {
    let notice = AdminNotice {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_owned(),
        lines,
        created_at: Utc::now().to_rfc3339(),
    };
    let mut notices = list_notices(catalog).await?;
    notices.push(notice.clone());
    save(catalog, &notices).await?;
    Ok(notice)
}

/// Dismiss the notice `id`, answering whether there was one.
pub async fn dismiss_notice(catalog: &Catalog, id: &str) -> Result<bool> {
    let mut notices = list_notices(catalog).await?;
    let before = notices.len();
    notices.retain(|n| n.id != id);
    if notices.len() == before {
        return Ok(false);
    }
    save(catalog, &notices).await?;
    Ok(true)
}

async fn save(catalog: &Catalog, notices: &[AdminNotice]) -> Result<()> {
    let value =
        serde_json::to_value(notices).map_err(|e| Error::msg(format!("admin notices: {e}")))?;
    sc_config::set_config(catalog, sc_config::ADMIN_NOTICES, value).await
}

/// The title of the row-level-security upgrade report.
pub const RLS_REPORT_TITLE: &str = "Row-level security is now a hard boundary: code that elevated \
                                    over these tables now sees what its cause sees";

/// Make the row-level-security upgrade report, once (TODO-type-hardening H8.6):
/// on the first start of this version, list in a notice and in the log every
/// code body and trigger that reaches an RLS table at elevated authority. A
/// later start does nothing, and an instance with nothing to report gets no
/// notice.
pub async fn report_rls_elevation(
    catalog: &Arc<Catalog>,
    dispatcher: &Arc<TriggerDispatcher>,
) -> Result<()> {
    if sc_config::config_value(catalog, sc_config::RLS_BOUNDARY_REPORTED).await? == Json::Bool(true)
    {
        return Ok(());
    }
    let lines = rls_elevation_report(catalog, dispatcher).await?;
    if !lines.is_empty() {
        sc_log::log_warn!("feldspar: {RLS_REPORT_TITLE}:");
        for line in &lines {
            sc_log::log_warn!("feldspar:   {line}");
        }
        add_notice(catalog, RLS_REPORT_TITLE, lines).await?;
    }
    sc_config::set_config(catalog, sc_config::RLS_BOUNDARY_REPORTED, Json::Bool(true)).await
}

/// The report's lines, from a static reading of the code and of the triggers'
/// definitions.
pub async fn rls_elevation_report(
    catalog: &Catalog,
    dispatcher: &TriggerDispatcher,
) -> Result<Vec<String>> {
    let rls: Vec<String> = catalog
        .tables()?
        .into_iter()
        .filter(|t| t.rls_enabled)
        .map(|t| t.name)
        .collect();
    if rls.is_empty() {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    for trigger in dispatcher.triggers()?.all() {
        lines.extend(trigger_lines(trigger, &rls));
    }
    for app in sc_app::list_applications(catalog).await? {
        for api in &app.apis {
            if api.provider != sc_api::REST_PROVIDER {
                continue;
            }
            let Ok(queries) = sc_api::custom_queries(&api.config) else {
                continue;
            };
            for query in queries {
                if query.language.action().is_none() {
                    // SQL: it runs at its caller's settings, never elevated, and
                    // H8.3 already refuses a statement that would forge them.
                    continue;
                }
                for table in elevated_tables(&query.code, &rls) {
                    lines.push(format!(
                        "custom query `{}` of application `{}` says `asAdmin()` and names `{table}`, \
                         which has row-level security: `asAdmin()` there is now refused",
                        query.name, app.subdomain
                    ));
                }
            }
        }
    }
    for library in sc_catalog::list_code_libraries(catalog)
        .await
        .unwrap_or_default()
    {
        for table in elevated_tables(&library.code, &rls) {
            lines.push(format!(
                "code library `{}` says `asAdmin()` and names `{table}`, which has row-level \
                 security: `asAdmin()` there is now refused",
                library.name
            ));
        }
    }
    Ok(lines)
}

/// What one trigger contributes: its own table, the RLS tables its code names
/// at its default (admin) authority, and the ones it names with `asAdmin()`.
/// A trigger no person's event fires (scheduled, startup, a stream element)
/// still runs at system authority, so it is not listed.
fn trigger_lines(trigger: &Trigger, rls: &[String]) -> Vec<String> {
    if matches!(
        trigger.when,
        EventKind::Startup
            | EventKind::Often
            | EventKind::Hourly
            | EventKind::Daily
            | EventKind::Weekly
            | EventKind::Stream
    ) {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let code = trigger
        .configuration()
        .and_then(|c| c.get("code"))
        .and_then(Json::as_str)
        .unwrap_or_default();
    let mut reached: Vec<&str> = Vec::new();
    if let Some(table) = trigger.channel.as_deref()
        && rls.iter().any(|t| t == table)
    {
        reached.push(table);
    }
    for table in rls {
        if names_table(code, table) && !reached.contains(&table.as_str()) {
            reached.push(table);
        }
    }
    for table in reached {
        lines.push(format!(
            "trigger `{}` runs at admin authority by default and reaches `{table}`, which has \
             row-level security: there it now sees what the user who caused it sees (a person \
             may approve it to run as system)",
            trigger.name
        ));
    }
    for table in elevated_tables(code, rls) {
        lines.push(format!(
            "trigger `{}`'s code says `asAdmin()` and names `{table}`, which has row-level \
             security: `asAdmin()` there is now refused",
            trigger.name
        ));
    }
    lines
}

/// The RLS tables `code` names, when it says `asAdmin()` at all.
fn elevated_tables<'a>(code: &str, rls: &'a [String]) -> Vec<&'a str> {
    if !code.contains("asAdmin(") {
        return Vec::new();
    }
    rls.iter()
        .filter(|table| names_table(code, table))
        .map(String::as_str)
        .collect()
}

/// Whether `code` names `table` the ways a body does: as a member,
/// `db.<table>` or `db.asAdmin().<table>`, or as a string (`db.table("t")`,
/// `Table.findOne('t')`, a template). A reading of the text, not of the
/// program, so it may list a field of that name too: the report errs towards
/// listing.
fn names_table(code: &str, table: &str) -> bool {
    // `.books` but not `.bookshelf`: the name ends where an identifier does.
    let member = format!(".{table}");
    let as_member = code.match_indices(&member).any(|(at, _)| {
        !code[at + member.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    });
    as_member
        || ['"', '\'', '`']
            .iter()
            .any(|q| code.contains(&format!("{q}{table}{q}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_names_a_table_by_handle_or_by_string() {
        assert!(names_table("await db.books.rows()", "books"));
        assert!(names_table("db.table('books')", "books"));
        assert!(names_table("Table.findOne(\"books\")", "books"));
        assert!(!names_table("await db.bookshelf.rows()", "books"));
        assert!(!names_table("return 1", "books"));
        let rls = vec!["books".to_owned(), "notes".to_owned()];
        assert_eq!(
            elevated_tables("await db.asAdmin().books.rows()", &rls),
            vec!["books"]
        );
        assert!(elevated_tables("await db.books.rows()", &rls).is_empty());
    }

    #[test]
    fn a_scheduled_trigger_is_not_listed_and_a_table_trigger_is() {
        let rls = vec!["books".to_owned()];
        let scheduled = Trigger::new("nightly", EventKind::Daily, "run_js_code")
            .config("code", "await db.asAdmin().books.rows()");
        assert!(trigger_lines(&scheduled, &rls).is_empty());
        let on_insert = Trigger::new("count", EventKind::Insert, "run_js_code")
            .on("books")
            .config("code", "await db.asAdmin().books.rows()");
        let lines = trigger_lines(&on_insert, &rls);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("now sees what the user who caused it sees"));
        assert!(lines[1].contains("is now refused"));
    }
}
