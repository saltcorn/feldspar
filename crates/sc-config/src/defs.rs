//! What configuration keys exist, and what each one is.
//!
//! A key is declared as a [`FormField`] — the vocabulary every configurable
//! thing in the system already speaks (§6.2) — grouped into a
//! [`ConfigSection`] so the settings screen has something to put a heading on.
//! That declaration is the *only* place a key is defined: it gives
//! [`crate::store`] the type to check a write against, gives the admin UI the
//! control to render, and gives the reader its default. Adding a setting is
//! adding one entry here.
//!
//! Sections are ordered as the screen shows them, and the fields within a
//! section as the form lays them out.

use std::sync::OnceLock;

use sc_types::FormField;

/// One configuration key: its declaration, plus the sentence under the control.
///
/// The help text is not part of [`FormField`] because a *label* is what every
/// consumer of that vocabulary needs and a paragraph is what only a settings
/// screen has room for — a file store's backend picker does not want one.
#[derive(Debug, Clone)]
pub struct ConfigDef {
    /// The key's name, type, default, options and `secret` flag.
    pub field: FormField,
    /// A sentence shown under the control. Empty when the label says it all.
    pub help: &'static str,
}

impl ConfigDef {
    /// Declare a key with no help text.
    pub fn new(field: FormField) -> ConfigDef {
        ConfigDef { field, help: "" }
    }

    /// Declare a key with the sentence that goes under its control.
    pub fn help(field: FormField, help: &'static str) -> ConfigDef {
        ConfigDef { field, help }
    }

    /// The key this definition declares.
    pub fn key(&self) -> &str {
        self.field.name()
    }
}

/// A group of keys shown together, with a heading and an explanation.
#[derive(Debug, Clone)]
pub struct ConfigSection {
    /// Stable identifier (`ssl`), used by the API and by tests.
    pub name: &'static str,
    /// The heading the screen shows.
    pub label: &'static str,
    /// A paragraph under the heading: what these settings do, and what taking
    /// effect requires.
    pub description: &'static str,
    /// The keys in this section, in form order.
    pub fields: Vec<ConfigDef>,
}

/// What the Backup screen includes: the selection an admin made last time,
/// stored so a tuned selection survives the dialog being closed.
///
/// [`internal_defs`] rather than a section, because it is not a setting anybody
/// types into a form — the shape is the Backup tab's, and the tab is what edits
/// it.
pub const BACKUP_INCLUDE: &str = "backup_include";

/// The automated backups: a list of `{ id, destination, frequency,
/// retention_days }`, written by the Backup tab's Automated backups card.
///
/// Internal for the reason [`BACKUP_INCLUDE`] is — a list of records is not a
/// form control.
pub const BACKUP_SCHEDULES: &str = "backup_schedules";

/// What each automated backup last did: `{ <schedule id>: { last_attempt_at,
/// last_success_at, last_error, last_file } }`.
///
/// A key of its own rather than fields on [`BACKUP_SCHEDULES`], because the two
/// have different writers: the admin edits the schedules and only the backup
/// scheduler writes this, so neither can overwrite the other's change with a
/// stale copy (the arrangement a trigger's `last_run_at` has).
pub const BACKUP_SCHEDULE_STATUS: &str = "backup_schedule_status";

/// Every section, in screen order.
pub fn config_sections() -> &'static [ConfigSection] {
    static SECTIONS: OnceLock<Vec<ConfigSection>> = OnceLock::new();
    SECTIONS.get_or_init(|| {
        vec![
            crate::ssl::ssl_section(),
            crate::email::email_section(),
            crate::development::development_section(),
            crate::localisation::localisation_section(),
            crate::maps::maps_section(),
        ]
    })
}

/// Keys that are stored configuration but belong to **no settings form**.
///
/// A key has to be declared to be stored at all ([`crate::store`] refuses an
/// undeclared one, which is what gives every value a type to be checked
/// against), but not every stored value is a setting an admin edits in a
/// generic form: the Backup tab's include-selection is written by the screen
/// that owns it, in the shape that screen defines. Declaring those here keeps
/// the write checkable and keeps them out of [`config_spec`] — the spec the
/// settings form renders and validates against — so no form grows a control for
/// a value it cannot edit.
pub fn internal_defs() -> &'static [ConfigDef] {
    static DEFS: OnceLock<Vec<ConfigDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        vec![
            ConfigDef::new(
                // `Json`, because the value is a record of lists and flags rather
                // than a scalar — the one shape a `FormField` accepts wholesale.
                FormField::new(BACKUP_INCLUDE, sc_types::BasicType::Json)
                    .label("What a backup includes"),
            ),
            ConfigDef::new(
                FormField::new(BACKUP_SCHEDULES, sc_types::BasicType::Json)
                    .label("Automated backups"),
            ),
            ConfigDef::new(
                FormField::new(BACKUP_SCHEDULE_STATUS, sc_types::BasicType::Json)
                    .label("What each automated backup last did"),
            ),
        ]
    })
}

/// Every key of every **section**'s [`FormField`], flattened — the spec a whole
/// settings *form* payload is validated against.
///
/// Deliberately not [`internal_defs`]: those are not on the form, so they are
/// neither rendered nor read back by it.
pub fn config_spec() -> Vec<FormField> {
    config_sections()
        .iter()
        .flat_map(|section| section.fields.iter().map(|def| def.field.clone()))
        .collect()
}

/// The declaration for `key`, if there is one — from the sections or from
/// [`internal_defs`], since both are keys the table may hold.
pub fn definition(key: &str) -> Option<FormField> {
    all_defs()
        .find(|def| def.key() == key)
        .map(|def| def.field.clone())
}

/// Every declared key's name, sections first.
pub fn known_keys() -> Vec<&'static str> {
    all_defs()
        .map(|def| def.field.name())
        // The declarations are `'static` (behind a `OnceLock`), so the names are
        // too — which is what lets an error message list them without cloning.
        .collect()
}

/// Every declaration this server understands: the sections' fields, then the
/// keys no form shows.
fn all_defs() -> impl Iterator<Item = &'static ConfigDef> {
    config_sections()
        .iter()
        .flat_map(|section| section.fields.iter())
        .chain(internal_defs().iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_declared_once() {
        let keys = known_keys();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate configuration key");
        assert_eq!(config_spec().len() + internal_defs().len(), keys.len());
    }

    /// The keys no form shows are storable — `definition` finds them, so a write
    /// is checked — but they are **not** in the form's spec, which is what keeps
    /// the settings screen from rendering a control for them.
    #[test]
    fn an_internal_key_is_declared_but_not_on_the_form() {
        assert!(definition(BACKUP_INCLUDE).is_some());
        assert!(known_keys().contains(&BACKUP_INCLUDE));
        assert!(!config_spec().iter().any(|f| f.name() == BACKUP_INCLUDE));
        for key in [BACKUP_SCHEDULES, BACKUP_SCHEDULE_STATUS] {
            assert!(known_keys().contains(&key));
            assert!(!config_spec().iter().any(|f| f.name() == key));
        }
    }

    #[test]
    fn a_key_is_found_by_name_and_a_typo_is_not() {
        assert!(definition(crate::ssl::SSL_MODE).is_some());
        assert!(definition("ssl-mode").is_none());
    }

    #[test]
    fn every_declared_key_has_a_label() {
        for section in config_sections() {
            assert!(!section.label.is_empty());
            for def in &section.fields {
                assert!(
                    !def.field.base.label.is_empty(),
                    "`{}` has no label",
                    def.key()
                );
            }
        }
    }
}
