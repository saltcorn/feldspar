//! Following a table rename into the definitions that name the table.
//!
//! The schema editor renames the table, its overlay rows, the keys onto it and
//! its row history (`sc_api::schema_edit`, `sc_catalog::rename_table_meta`).
//! Everything else that names a table by name is owned by a crate the editor
//! cannot name, so the catalog calls back here, through
//! [`TableRenameHook`], once the rename has committed:
//!
//! - **applications**: the table subset, and the per-table read floors a
//!   Saltcorn UI application keeps for tables outside it;
//! - **views**: the table a view is over, and the relations its configuration
//!   follows (`ChildList:Books list.books.author`, an aggregation's
//!   `agg_relation`) — in pages' and the library's layouts too;
//! - **triggers**: the table a table event listens on, its "only if" formula,
//!   and the `table` its configured action works on;
//! - **workflows**, **agents**, **streams** and **datasets**: the `table` their
//!   steps, traits and configurations name, and a dataset's base and joins;
//! - **models**: the table a model was fitted over.
//!
//! A formula anywhere in those configurations that aggregates over the table
//! (`booksↃauthor`) is rewritten too. **Code is not**: a code query, a code
//! action, a code library or an application's own source naming the table is
//! the developer's to change, and the editor's notes say so.
//!
//! The rows are rewritten in place rather than saved through each store's
//! `save_*`, deliberately: a save validates the whole definition and some
//! create a new version, and a rename changes one name, not the definition. The
//! live sets that hold copies — the trigger dispatcher, the stream supervisor,
//! the mounted applications — are reloaded afterwards.

use sc_app::Application;
use sc_catalog::{Catalog, TableRenameHook};
use sc_db::Row;
use sc_error::Result;
use sc_query::{Assignment, Expr, Select, Source, Statement, Update, Value};
use serde_json::Value as Json;

use crate::apps::AppMounts;

/// The relation prefixes a v1 view reference carries ahead of the view's name
/// and a dot-separated path whose first step is a table.
const RELATION_PREFIXES: [&str; 3] = ["ChildList:", "ParentShow:", "OneToOneShow:"];

/// The configuration keys whose string value is a table's name.
const TABLE_KEYS: [&str; 1] = ["table"];

/// The configuration keys whose string value is code, which is left alone.
const CODE_KEYS: [&str; 3] = ["code", "source", "js"];

#[async_trait::async_trait]
impl TableRenameHook for AppMounts {
    async fn table_renamed(&self, catalog: &Catalog, from: &str, to: &str) -> Result<Vec<String>> {
        let rename = Rename { from, to };
        let mut notes = Vec::new();
        let mut apps_changed = rename.applications(catalog).await?;
        let views = rename.views(catalog).await?;
        let triggers = rename.triggers(catalog).await?;
        let workflows = rename
            .json_columns(
                catalog,
                sc_workflow::VERSIONS_TABLE,
                &[sc_workflow::versions::COL_STEPS],
            )
            .await?;
        let agents = rename
            .json_columns(
                catalog,
                sc_agent::AGENTS_TABLE,
                &[sc_agent::store::COL_TRAITS],
            )
            .await?;
        let streams = rename
            .json_columns(
                catalog,
                sc_stream::STREAMS_TABLE,
                &[sc_stream::store::COL_CONFIGURATION],
            )
            .await?;
        let datasets = rename
            .json_columns(
                catalog,
                sc_dataset::DATASETS_TABLE,
                &[sc_dataset::COL_BASE, sc_dataset::COL_OPERATIONS],
            )
            .await?;
        let models = rename
            .text_column(catalog, sc_model::MODELS_TABLE, sc_model::COL_TABLE_NAME)
            .await?;

        for (count, what) in [
            (views.len(), "view"),
            (triggers, "trigger"),
            (workflows, "workflow version"),
            (agents, "agent"),
            (streams, "stream"),
            (datasets, "dataset"),
            (models, "model"),
        ] {
            if count > 0 {
                notes.push(format!(
                    "{count} {what}{} now name{} `{to}`",
                    if count == 1 { "" } else { "s" },
                    if count == 1 { "s" } else { "" },
                ));
            }
        }
        if !apps_changed.is_empty() {
            notes.push(format!(
                "the table subset of {} now names `{to}`",
                apps_changed
                    .iter()
                    .map(|a| format!("`{}`", a.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // The live sets that hold copies of what was rewritten.
        if triggers > 0
            && let Some(dispatcher) = self.triggers()
            && let Err(e) = dispatcher.reload(catalog).await
        {
            notes.push(format!(
                "reloading the triggers after the rename failed: {e}"
            ));
        }
        if streams > 0
            && let Some(set) = self.streams()
            && let Err(e) = set.reload(catalog).await
        {
            notes.push(format!(
                "reloading the streams after the rename failed: {e}"
            ));
        }
        // An application is mounted again when its subset changed or one of its
        // views did: a Saltcorn UI application serves its views from a snapshot
        // taken when it was mounted.
        for app in sc_app::list_applications(catalog).await? {
            if views.contains(&app.id.0) && !apps_changed.iter().any(|a| a.id == app.id) {
                apps_changed.push(app);
            }
        }
        for app in apps_changed {
            if self.get(&app.subdomain).is_none() {
                continue;
            }
            let name = app.name.clone();
            if let Err(e) = crate::apps::mount_from_disk(self, app).await {
                notes.push(format!(
                    "application `{name}` could not be mounted again after the rename: {e}"
                ));
            }
        }
        Ok(notes)
    }
}

/// One rename, and the rewrites it makes.
struct Rename<'a> {
    from: &'a str,
    to: &'a str,
}

impl Rename<'_> {
    /// The applications whose table subset (or per-table read floors) named the
    /// table, rewritten and saved; returned as they now are.
    async fn applications(&self, catalog: &Catalog) -> Result<Vec<Application>> {
        let mut changed = Vec::new();
        if catalog.get(sc_app::APPLICATIONS_TABLE)?.is_none() {
            return Ok(changed);
        }
        for row in all_rows(catalog, sc_app::APPLICATIONS_TABLE).await? {
            let mut assignments = Vec::new();
            if let Some(mut tables) = json_of(&row, sc_app::COL_TABLES) {
                let mut hit = false;
                if let Json::Array(items) = &mut tables {
                    for item in items.iter_mut() {
                        if item.as_str() == Some(self.from) {
                            *item = Json::String(self.to.to_owned());
                            hit = true;
                        }
                    }
                }
                if hit {
                    assignments.push(json_assignment(sc_app::COL_TABLES, tables));
                }
            }
            if let Some(mut framework) = json_of(&row, sc_app::COL_FRAMEWORK) {
                let floors = framework
                    .get_mut("config")
                    .and_then(|c| c.get_mut("exttables_min_role_read"))
                    .and_then(Json::as_object_mut);
                if let Some(floors) = floors
                    && let Some(floor) = floors.remove(self.from)
                {
                    floors.insert(self.to.to_owned(), floor);
                    assignments.push(json_assignment(sc_app::COL_FRAMEWORK, framework));
                }
            }
            if !assignments.is_empty() {
                update_row(catalog, sc_app::APPLICATIONS_TABLE, &row, assignments).await?;
            }
        }
        // Read back through the store, so what is mounted is what is stored.
        for app in sc_app::list_applications(catalog).await? {
            if app.tables.iter().any(|t| t.0 == self.to) {
                changed.push(app);
            }
        }
        Ok(changed)
    }

    /// The views over the table or whose configuration follows a relation
    /// through it, and the pages and library entries whose layouts do; answers
    /// the applications of the views it rewrote.
    async fn views(&self, catalog: &Catalog) -> Result<Vec<uuid::Uuid>> {
        use sc_viewpattern as v;
        let mut apps = Vec::new();
        if catalog.get(v::VIEWS_TABLE)?.is_some() {
            for row in all_rows(catalog, v::VIEWS_TABLE).await? {
                let mut assignments = Vec::new();
                if matches!(row.get(v::COL_TABLE_NAME), Some(Value::Text(t)) if t == self.from) {
                    assignments.push(Assignment::new(
                        v::COL_TABLE_NAME.to_owned(),
                        Expr::Lit(Value::Text(self.to.to_owned())),
                    ));
                }
                if let Some(mut config) = json_of(&row, v::COL_CONFIGURATION)
                    && self.view_json(&mut config)
                {
                    assignments.push(json_assignment(v::COL_CONFIGURATION, config));
                }
                if !assignments.is_empty() {
                    if let Some(Value::Uuid(app)) = row.get(v::COL_APPLICATION)
                        && !apps.contains(app)
                    {
                        apps.push(*app);
                    }
                    update_row(catalog, v::VIEWS_TABLE, &row, assignments).await?;
                }
            }
        }
        for table in [v::PAGES_TABLE, v::LIBRARY_TABLE] {
            if catalog.get(table)?.is_none() {
                continue;
            }
            for row in all_rows(catalog, table).await? {
                if let Some(mut layout) = json_of(&row, v::COL_LAYOUT)
                    && self.view_json(&mut layout)
                {
                    if let Some(Value::Uuid(app)) = row.get(v::COL_APPLICATION)
                        && !apps.contains(app)
                    {
                        apps.push(*app);
                    }
                    update_row(
                        catalog,
                        table,
                        &row,
                        vec![json_assignment(v::COL_LAYOUT, layout)],
                    )
                    .await?;
                }
            }
        }
        Ok(apps)
    }

    /// The triggers on the table, and those whose action or "only if" formula
    /// names it. Answers how many changed.
    async fn triggers(&self, catalog: &Catalog) -> Result<usize> {
        use sc_action as t;
        if catalog.get(t::TRIGGERS_TABLE)?.is_none() {
            return Ok(0);
        }
        let mut count = 0;
        for row in all_rows(catalog, t::TRIGGERS_TABLE).await? {
            let mut assignments = Vec::new();
            let table_event = matches!(
                row.get(t::COL_EVENT),
                Some(Value::Text(e)) if sc_action::EventKind::parse(e).is_ok_and(|k| k.is_table_event())
            );
            if table_event
                && matches!(row.get(t::COL_CHANNEL), Some(Value::Text(c)) if c == self.from)
            {
                assignments.push(text_assignment(t::COL_CHANNEL, self.to));
            }
            if let Some(Value::Text(only_if)) = row.get(t::COL_ONLY_IF)
                && let Some(renamed) = sc_expr::rename_relation_table(only_if, self.from, self.to)
            {
                assignments.push(text_assignment(t::COL_ONLY_IF, &renamed));
            }
            if let Some(mut config) = json_of(&row, t::COL_CONFIGURATION)
                && self.config_json(&mut config)
            {
                assignments.push(json_assignment(t::COL_CONFIGURATION, config));
            }
            if !assignments.is_empty() {
                update_row(catalog, t::TRIGGERS_TABLE, &row, assignments).await?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Rewrite the configuration JSON in `columns` of every row of `table`.
    /// Answers how many rows changed.
    async fn json_columns(
        &self,
        catalog: &Catalog,
        table: &str,
        columns: &[&str],
    ) -> Result<usize> {
        if catalog.get(table)?.is_none() {
            return Ok(0);
        }
        let mut count = 0;
        for row in all_rows(catalog, table).await? {
            let mut assignments = Vec::new();
            for column in columns {
                if let Some(mut value) = json_of(&row, column)
                    && self.config_json(&mut value)
                {
                    assignments.push(json_assignment(column, value));
                }
            }
            if !assignments.is_empty() {
                update_row(catalog, table, &row, assignments).await?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Rewrite a text `column` holding the table's name in every row of `table`.
    async fn text_column(&self, catalog: &Catalog, table: &str, column: &str) -> Result<usize> {
        if catalog.get(table)?.is_none() {
            return Ok(0);
        }
        let update = Update::new(
            table,
            vec![Assignment::new(column.to_owned(), Expr::lit(self.to))],
        )
        .filter(Expr::col(column).eq(Expr::lit(self.from)));
        let before = all_rows(catalog, table)
            .await?
            .iter()
            .filter(|r| matches!(r.get(column), Some(Value::Text(t)) if t == self.from))
            .count();
        if before > 0 {
            run(catalog, Statement::from(update)).await?;
        }
        Ok(before)
    }

    /// Rewrite a definition's configuration: every `table` naming the renamed
    /// table, the generated-code trait's newline-separated `tables`, and every
    /// formula aggregating over it. Code is left as written. Answers whether
    /// anything changed.
    fn config_json(&self, value: &mut Json) -> bool {
        match value {
            Json::Object(map) => {
                let mut changed = false;
                for (key, child) in map.iter_mut() {
                    if CODE_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    if let Json::String(text) = child {
                        if TABLE_KEYS.contains(&key.as_str()) && text == self.from {
                            *text = self.to.to_owned();
                            changed = true;
                        } else if key == "tables" && text.lines().any(|l| l.trim() == self.from) {
                            *text = text
                                .lines()
                                .map(|l| if l.trim() == self.from { self.to } else { l })
                                .collect::<Vec<_>>()
                                .join("\n");
                            changed = true;
                        } else if let Some(renamed) =
                            sc_expr::rename_relation_table(text, self.from, self.to)
                        {
                            *text = renamed;
                            changed = true;
                        }
                    } else {
                        changed |= self.config_json(child);
                    }
                }
                changed
            }
            // Every item, not the first that changes: `any` would stop there.
            Json::Array(items) => {
                items
                    .iter_mut()
                    .map(|item| self.config_json(item))
                    .filter(|changed| *changed)
                    .count()
                    > 0
            }
            _ => false,
        }
    }

    /// Rewrite a view configuration or a layout: what [`config_json`] does,
    /// plus the relations a v1 view reference follows (in values and in the keys
    /// of `subtables`) and an aggregation's `agg_relation`.
    ///
    /// [`config_json`]: Rename::config_json
    fn view_json(&self, value: &mut Json) -> bool {
        match value {
            Json::Object(map) => {
                let mut changed = false;
                let keys: Vec<String> = map.keys().cloned().collect();
                for key in keys {
                    if let Some(renamed) = self.relation(&key)
                        && let Some(child) = map.remove(&key)
                    {
                        map.insert(renamed, child);
                        changed = true;
                    }
                }
                for (key, child) in map.iter_mut() {
                    if CODE_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    match child {
                        Json::String(text) => {
                            let renamed = if key == "agg_relation" {
                                self.agg_relation(text)
                            } else if TABLE_KEYS.contains(&key.as_str()) && text == self.from {
                                Some(self.to.to_owned())
                            } else {
                                self.relation(text).or_else(|| {
                                    sc_expr::rename_relation_table(text, self.from, self.to)
                                })
                            };
                            if let Some(renamed) = renamed {
                                *text = renamed;
                                changed = true;
                            }
                        }
                        other => changed |= self.view_json(other),
                    }
                }
                changed
            }
            // Every item, not the first that changes: `any` would stop there.
            Json::Array(items) => {
                items
                    .iter_mut()
                    .map(|item| self.view_json(item))
                    .filter(|changed| *changed)
                    .count()
                    > 0
            }
            _ => false,
        }
    }

    /// `ChildList:Books list.books.author` with the table step renamed, or
    /// `None` when it does not go through the renamed table.
    fn relation(&self, text: &str) -> Option<String> {
        let prefix = RELATION_PREFIXES.iter().find(|p| text.starts_with(**p))?;
        let mut parts: Vec<&str> = text[prefix.len()..].split('.').collect();
        if parts.len() < 3 || parts[1] != self.from {
            return None;
        }
        parts[1] = self.to;
        Some(format!("{prefix}{}", parts.join(".")))
    }

    /// An aggregation's `table.key` (or `through->table.key`) with the table
    /// renamed, or `None` when it is not the renamed table.
    fn agg_relation(&self, text: &str) -> Option<String> {
        let (through, rest) = match text.split_once("->") {
            Some((through, rest)) => (Some(through), rest),
            None => (None, text),
        };
        let (table, key) = rest.split_once('.')?;
        if table != self.from {
            return None;
        }
        Some(match through {
            Some(through) => format!("{through}->{}.{key}", self.to),
            None => format!("{}.{key}", self.to),
        })
    }
}

/// Every row of `table`, whole.
async fn all_rows(catalog: &Catalog, table: &str) -> Result<Vec<Row>> {
    catalog
        .primary(crate::ADMIN_TASKS)
        .query(&Statement::from(Select::from(Source::table(table))))
        .await?
        .try_collect()
        .await
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(crate::ADMIN_TASKS)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Write `assignments` to `row`, found again by its `id`.
async fn update_row(
    catalog: &Catalog,
    table: &str,
    row: &Row,
    assignments: Vec<Assignment>,
) -> Result<()> {
    let id = row
        .get("id")
        .cloned()
        .ok_or_else(|| sc_error::Error::msg(format!("a row of `{table}` has no id")))?;
    let update = Update::new(table, assignments).filter(Expr::col("id").eq(Expr::Lit(id)));
    run(catalog, Statement::from(update)).await
}

/// A JSON column's value, whichever way the driver hands it back.
fn json_of(row: &Row, column: &str) -> Option<Json> {
    match row.get(column)? {
        Value::Json(json) => Some(json.clone()),
        Value::Text(text) => serde_json::from_str(text).ok(),
        _ => None,
    }
}

fn json_assignment(column: &str, value: Json) -> Assignment {
    Assignment::new(column.to_owned(), Expr::Lit(Value::Json(value)))
}

fn text_assignment(column: &str, value: &str) -> Assignment {
    Assignment::new(column.to_owned(), Expr::Lit(Value::Text(value.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RENAME: Rename<'static> = Rename {
        from: "books",
        to: "titles",
    };

    #[test]
    fn a_configuration_names_the_new_table_and_keeps_its_code() {
        let mut config = json!({
            "table": "books",
            "where": "booksↃauthor.length > 0",
            "steps": [{ "configuration": { "table": "books", "code": "db.books.find()" } }],
            "tables": "authors\nbooks",
            "other": "books",
        });
        assert!(RENAME.config_json(&mut config));
        assert_eq!(
            config,
            json!({
                "table": "titles",
                "where": "titlesↃauthor.length > 0",
                "steps": [{ "configuration": { "table": "titles", "code": "db.books.find()" } }],
                "tables": "authors\ntitles",
                "other": "books",
            })
        );
        let mut untouched = json!({ "table": "authors" });
        assert!(!RENAME.config_json(&mut untouched));
    }

    #[test]
    fn a_view_follows_its_relations_through_the_new_table() {
        let mut config = json!({
            "columns": [
                { "type": "Aggregation", "agg_relation": "books.author", "agg_field": "pages" },
                { "type": "Aggregation", "agg_relation": "loans->books.loan" },
                { "type": "ViewLink", "view": "ChildList:Book list.books.author" },
                { "type": "ViewLink", "view": "ChildList:Loan list.loans.book" },
            ],
            "subtables": { "ChildList:Book show.books.author": true },
        });
        assert!(RENAME.view_json(&mut config));
        assert_eq!(
            config,
            json!({
                "columns": [
                    { "type": "Aggregation", "agg_relation": "titles.author", "agg_field": "pages" },
                    { "type": "Aggregation", "agg_relation": "loans->titles.loan" },
                    { "type": "ViewLink", "view": "ChildList:Book list.titles.author" },
                    { "type": "ViewLink", "view": "ChildList:Loan list.loans.book" },
                ],
                "subtables": { "ChildList:Book show.titles.author": true },
            })
        );
    }
}
