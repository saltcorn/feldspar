//! The `_fd_config` table: one row per configuration value (design §9).
//!
//! A configuration value is **a key and a JSON value**, and the key is not free
//! text: every key this server understands is declared in [`crate::defs`] as a
//! [`FormField`], which is what gives it a type, a label, a default, an optional
//! list of allowed values and a `secret` flag. Writing is checked against that
//! declaration ([`set_config`]), so a value of the wrong type never reaches the
//! table and a key nobody declared is refused rather than stored where it would
//! do nothing.
//!
//! **Reading is strict too.** A stored value that no longer matches its
//! declaration is an [`Error::invalid`] naming the key, not a silently ignored
//! row: the only way to produce one is to change a declaration under a running
//! deployment, and a setting the admin believes they set doing nothing is the
//! failure this whole arrangement exists to prevent. [`delete_config`] is the
//! way out.

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::Value as Json;

use crate::defs::{definition, known_keys};

/// Name of the configuration table in the primary database.
pub const CONFIG_TABLE: &str = "_fd_config";

/// The key column — the primary key, and the name a declaration is found by.
pub const COL_KEY: &str = "key";
/// The value column: JSON, whatever the key's declared type renders as.
pub const COL_VALUE: &str = "value";

/// The fields of `_fd_config`, in declaration order.
///
/// The value is JSON rather than one column per type because the *type* lives in
/// the declaration, not in the table (§9: "per-key value-type restriction; values
/// stored as JSON"). A `bool` key holds `true`, an `int` key holds `8443`, and
/// both are read back as what the declaration says they are.
fn config_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_KEY, TypeRef::Basic(BasicType::Text))
            .required()
            .primary_key(),
        DataField::plain(COL_VALUE, TypeRef::Basic(BasicType::Json)).required(),
    ]
}

/// Ensure `_fd_config` exists, creating it if absent.
///
/// Idempotent, like every other bootstrap; call once at startup after the
/// [`Catalog`] is initialised.
pub async fn bootstrap_config(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(CONFIG_TABLE, &config_fields())
        .await
}

/// The stored value for `key`, or `None` when nothing is stored.
///
/// The *declared default* is not applied here — use [`config_value`] for the
/// value the server should act on, and this to tell "unset" from "set to the
/// same thing as the default".
pub async fn stored_config(catalog: &Catalog, key: &str) -> Result<Option<Json>> {
    let field = require_definition(key)?;
    let select = Select::from(Source::table(CONFIG_TABLE))
        .filter(Expr::col(COL_KEY).eq(Expr::lit(key)))
        .limit(1);
    let Some(row) = rows(catalog, select).await?.into_iter().next() else {
        return Ok(None);
    };
    let value = value_of(&row, key)?;
    check(&field, &value)?;
    Ok(Some(value))
}

/// The value the server should act on for `key`: what this host pins
/// ([`set_host_config`]), else what is stored, else the declared default, else
/// `Json::Null` for a key with none of them.
pub async fn config_value(catalog: &Catalog, key: &str) -> Result<Json> {
    let field = require_definition(key)?;
    if let Some(pinned) = catalog.host_config().get(key) {
        return Ok(pinned.clone());
    }
    Ok(stored_config(catalog, key)
        .await?
        .or_else(|| field.default.clone())
        .unwrap_or(Json::Null))
}

/// Every declared key with the value the server acts on — pinned by this host
/// where it is ([`set_host_config`]), stored where set, declared default where
/// not, absent where there is none of them.
///
/// This is what the settings screen renders and what the boot path reads its
/// serving settings out of, so the two cannot disagree about what a setting is.
pub async fn all_config(catalog: &Catalog) -> Result<Attrs> {
    let stored = stored_rows(catalog).await?;
    let pinned = catalog.host_config();
    let mut out = Attrs::new();
    for field in crate::defs::config_spec() {
        let key = field.name();
        if let Some(value) = pinned.get(key) {
            out.insert(key.to_owned(), value.clone());
            continue;
        }
        let value = match stored.get(key) {
            Some(value) => {
                check(&field, value)?;
                value.clone()
            }
            None => match &field.default {
                Some(default) => default.clone(),
                None => continue,
            },
        };
        out.insert(key.to_owned(), value);
    }
    Ok(out)
}

/// Keys in the table that no declaration describes.
///
/// Nothing reads such a row, so it is not an error that stops the server — but
/// it is not nothing either (it is a setting somebody believes is in force), so
/// it is reportable rather than invisible.
pub async fn stray_config_keys(catalog: &Catalog) -> Result<Vec<String>> {
    let known = known_keys();
    let mut stray: Vec<String> = stored_rows(catalog)
        .await?
        .into_keys()
        .filter(|key| !known.contains(&key.as_str()))
        .collect();
    stray.sort();
    Ok(stray)
}

/// Set one configuration value, checked against its declaration.
///
/// A `null` **clears** the setting (the row is deleted), which is how a form
/// returns a key to its default. Everything else is validated first: an unknown
/// key, a value of the wrong type, or a value outside the declared options is an
/// [`Error::invalid`] naming the key, and nothing is written.
///
/// A key this host pins ([`set_host_config`]) is refused unless the value is
/// the pinned one, which writes nothing: the file wins, and storing a value
/// that would never be read is a setting somebody believes they changed.
pub async fn set_config(catalog: &Catalog, key: &str, value: Json) -> Result<()> {
    let field = require_definition(key)?;
    if value.is_null() {
        delete_config(catalog, key).await?;
        return Ok(());
    }
    check(&field, &value)?;
    if pinned_unchanged(&catalog.host_config(), key, &value)? {
        return Ok(());
    }
    write_value(catalog, key, &value).await
}

/// Set several values in one go, checking **all** of them before writing any.
///
/// Checking first is what makes a settings form's Save an all-or-nothing act: a
/// bad private key does not leave the mode switched to `custom` with nothing to
/// serve. (The writes are separate statements, so this is not atomic against a
/// concurrent save of the same keys — the database's row locks decide that, and
/// two admins saving the same screen at once is the same race as two saving any
/// other record.)
///
/// A key this host pins is treated as [`set_config`] treats it: the pinned
/// value is skipped (a settings form sends back what it was shown), any other
/// is refused before anything is written.
pub async fn set_config_many(catalog: &Catalog, values: &Attrs) -> Result<()> {
    let pinned = catalog.host_config();
    let mut skip = Vec::new();
    for (key, value) in values {
        let field = require_definition(key)?;
        if !value.is_null() {
            check(&field, value)?;
            if pinned_unchanged(&pinned, key, value)? {
                skip.push(key.as_str());
            }
        }
    }
    for (key, value) in values {
        if skip.contains(&key.as_str()) {
            continue;
        }
        if value.is_null() {
            delete_config(catalog, key).await?;
        } else {
            write_value(catalog, key, value).await?;
        }
    }
    Ok(())
}

/// Pin `values` over `_fd_config` for as long as this process runs — the TLS
/// keys an `[environments.*]` section of `feldspar.toml` gives (§13.5).
///
/// A pinned key **wins**: [`all_config`] and [`config_value`] return it whatever
/// is stored, the settings screen shows it read-only ([`host_config_keys`]), a
/// save that would change it is refused, and a restore or Clear all — which
/// work on the table — cannot reach it. Only the keys in
/// [`crate::ssl::HOST_KEYS`] may be pinned, and each is checked against its
/// declaration here, so a mistyped mode in the file stops the boot with the
/// key's name rather than being found by a browser that cannot connect.
pub fn set_host_config(catalog: &Catalog, values: Attrs) -> Result<()> {
    for (key, value) in &values {
        if !crate::ssl::HOST_KEYS.contains(&key.as_str()) {
            return Err(Error::invalid(format!(
                "configuration key `{key}` cannot be set by the host; the keys that can \
                 are {}",
                crate::ssl::HOST_KEYS.join(", ")
            )));
        }
        check(&require_definition(key)?, value)?;
    }
    catalog.set_host_config(values);
    Ok(())
}

/// The keys this host pins, in declaration order — what the settings screen
/// shows read-only.
pub fn host_config_keys(catalog: &Catalog) -> Vec<String> {
    let pinned = catalog.host_config();
    crate::defs::known_keys()
        .into_iter()
        .filter(|key| pinned.contains_key(*key))
        .map(str::to_owned)
        .collect()
}

/// Whether `key` is pinned to exactly `value` (nothing to write); an error when
/// it is pinned to something else; `false` when it is not pinned.
fn pinned_unchanged(pinned: &Attrs, key: &str, value: &Json) -> Result<bool> {
    match pinned.get(key) {
        None => Ok(false),
        Some(held) if held == value => Ok(true),
        Some(_) => Err(Error::invalid(format!(
            "`{key}` is set in this host's feldspar.toml, so it cannot be changed here; \
             change it in the file and restart the server"
        ))),
    }
}

/// Delete a stored value, returning whether there was one. The key returns to
/// its declared default.
///
/// Undeclared keys are deletable on purpose: this is the way to clear a
/// [`stray`](stray_config_keys) row left by a key that no longer exists.
pub async fn delete_config(catalog: &Catalog, key: &str) -> Result<bool> {
    let existed = raw_stored(catalog, key).await?.is_some();
    let delete = Delete::from(CONFIG_TABLE).filter(Expr::col(COL_KEY).eq(Expr::lit(key)));
    run(catalog, Statement::from(delete)).await?;
    Ok(existed)
}

/// The declaration for `key`, or an error naming what is declared.
fn require_definition(key: &str) -> Result<FormField> {
    definition(key).ok_or_else(|| {
        Error::invalid(format!(
            "no configuration key `{key}`; known keys are {}",
            known_keys().join(", ")
        ))
    })
}

/// Check one value against its declaration, by validating a one-key bag.
///
/// [`FormField::validate`] is the same check every other configurable thing goes
/// through (§6.2), so `int`, `bool`, options and required-ness mean here exactly
/// what they mean in a file store's settings.
fn check(field: &FormField, value: &Json) -> Result<()> {
    let mut attrs = Attrs::new();
    attrs.insert(field.name().to_owned(), value.clone());
    field.validate(&attrs)
}

/// Insert or update the row for `key`.
async fn write_value(catalog: &Catalog, key: &str, value: &Json) -> Result<()> {
    let stored = Value::Json(value.clone());
    if raw_stored(catalog, key).await?.is_some() {
        let update = Update::new(
            CONFIG_TABLE,
            vec![Assignment::new(COL_VALUE, Expr::Lit(stored))],
        )
        .filter(Expr::col(COL_KEY).eq(Expr::lit(key)));
        run(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            CONFIG_TABLE,
            vec![COL_KEY.to_owned(), COL_VALUE.to_owned()],
            vec![Expr::lit(key), Expr::Lit(stored)],
        );
        run(catalog, Statement::from(insert)).await
    }
}

/// The stored value for `key` without consulting any declaration — what the
/// insert/update decision and [`delete_config`] need.
async fn raw_stored(catalog: &Catalog, key: &str) -> Result<Option<Json>> {
    let select = Select::from(Source::table(CONFIG_TABLE))
        .filter(Expr::col(COL_KEY).eq(Expr::lit(key)))
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(value_of(row, key)?)),
        None => Ok(None),
    }
}

/// Every row in the table, as key → stored JSON.
async fn stored_rows(catalog: &Catalog) -> Result<std::collections::BTreeMap<String, Json>> {
    let select = Select::from(Source::table(CONFIG_TABLE));
    let mut out = std::collections::BTreeMap::new();
    for row in rows(catalog, select).await? {
        let key = match row.get(COL_KEY) {
            Some(Value::Text(t)) => t.clone(),
            other => {
                return Err(bad_column(COL_KEY, "text", other));
            }
        };
        let value = value_of(&row, &key)?;
        out.insert(key, value);
    }
    Ok(out)
}

/// The `value` column of a row, as JSON.
///
/// A driver that hands back a plain scalar rather than a JSON value is accepted:
/// the column is JSON, but a backend is free to render `true` or `8443` as its
/// own type, and refusing that would be refusing the value we just wrote.
fn value_of(row: &Row, key: &str) -> Result<Json> {
    match row.get(COL_VALUE) {
        Some(Value::Json(json)) => Ok(json.clone()),
        Some(Value::Text(t)) => Ok(Json::String(t.clone())),
        Some(Value::Bool(b)) => Ok(Json::Bool(*b)),
        Some(Value::Int(i)) => Ok(Json::from(*i)),
        Some(Value::Float(f)) => Ok(Json::from(*f)),
        Some(Value::Null) | None => Err(Error::invalid(format!(
            "configuration key `{key}` has no value"
        ))),
        other => Err(bad_column(COL_VALUE, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{CONFIG_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_table_is_a_hidden_system_table_keyed_by_the_setting() {
        assert!(CONFIG_TABLE.starts_with("_fd_"));
        let fields = config_fields();
        let key = &fields[0];
        assert!(key.primary_key && key.required);
        assert_eq!(key.base.type_, TypeRef::Basic(BasicType::Text));
        assert_eq!(fields[1].base.type_, TypeRef::Basic(BasicType::Json));
    }

    #[test]
    fn an_unknown_key_names_what_is_known() {
        let err = require_definition("ssl_mode_typo").unwrap_err().to_string();
        assert!(err.contains("ssl_mode_typo"), "{err}");
        assert!(
            err.contains("ssl_mode"),
            "should list the known keys: {err}"
        );
    }

    /// The whole point of the declaration: the type is checked before the write,
    /// so `smtp_port = "yes"` is a message rather than a row.
    #[test]
    fn a_value_is_checked_against_its_declared_type() {
        let port = require_definition(crate::email::SMTP_PORT).unwrap();
        assert!(check(&port, &json!(2525)).is_ok());
        let err = check(&port, &json!("yes")).unwrap_err().to_string();
        assert!(err.contains(crate::email::SMTP_PORT), "{err}");

        let mode = require_definition(crate::ssl::SSL_MODE).unwrap();
        assert!(check(&mode, &json!("letsencrypt")).is_ok());
        let err = check(&mode, &json!("sortof")).unwrap_err().to_string();
        assert!(err.contains("letsencrypt"), "should list options: {err}");
    }
}
