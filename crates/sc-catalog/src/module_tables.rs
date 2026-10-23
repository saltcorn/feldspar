//! Which tables an installed **module** declared (TODO-imap M4.3), and which of
//! their fields.
//!
//! A module that declares tables (`sc_module::tables`) gets ordinary tables:
//! triggers, live streams, REST and ownership all work on them as on any other.
//! What is different is that the module's code depends on the fields it
//! declared, so while it is installed those fields — and the tables — must not
//! be dropped from under it. The admin may still add fields, edit rows and set
//! triggers.
//!
//! **Nothing is stored.** The claim is read from the module's package when it
//! loads and installed here whole on every module reload, the way the module
//! functions and the table providers are. That gives "while the module is
//! installed" for free: an uninstalled module is not in the next set, so its
//! tables become ordinary tables an admin may change, and nothing was dropped.

use std::collections::BTreeMap;

use sc_error::{Error, Result};

/// One module's claim on one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleTableClaim {
    /// The module's package name — `@feldspar/imap`.
    pub module: String,
    /// The fields the module declared, the primary key among them. A field an
    /// admin added is not here, and stays theirs.
    pub fields: Vec<String>,
}

/// Every installed module's claim, by table name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleTables {
    claims: BTreeMap<String, ModuleTableClaim>,
}

impl ModuleTables {
    /// No claims — a server with no module that declares tables.
    pub fn new() -> ModuleTables {
        ModuleTables::default()
    }

    /// Record that `module` declares `table` with `fields`.
    ///
    /// A table two modules both declare stays with the first: the second's
    /// load reports the clash on its own card (`sc_module::tables`), and which
    /// module a table belongs to must not depend on the order of a list.
    pub fn claim(
        &mut self,
        table: impl Into<String>,
        module: impl Into<String>,
        fields: Vec<String>,
    ) -> bool {
        let table = table.into();
        if self.claims.contains_key(&table) {
            return false;
        }
        self.claims.insert(
            table,
            ModuleTableClaim {
                module: module.into(),
                fields,
            },
        );
        true
    }

    /// The claim on `table`, if a module made one.
    pub fn get(&self, table: &str) -> Option<&ModuleTableClaim> {
        self.claims.get(table)
    }

    /// The module that declared `table`.
    pub fn module_of(&self, table: &str) -> Option<&str> {
        self.claims.get(table).map(|c| c.module.as_str())
    }

    /// Whether `field` of `table` is one a module declared.
    pub fn declares_field(&self, table: &str, field: &str) -> bool {
        self.claims
            .get(table)
            .is_some_and(|c| c.fields.iter().any(|f| f == field))
    }

    /// Every claim, in table-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ModuleTableClaim)> {
        self.claims.iter().map(|(t, c)| (t.as_str(), c))
    }

    /// Whether no module declares any table.
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// Refuse dropping `table`, or its field `field`, while a module declares
    /// it. `Ok` for a table no module declared and for a field an admin added.
    pub fn refuse_drop(&self, table: &str, field: Option<&str>) -> Result<()> {
        let Some(claim) = self.claims.get(table) else {
            return Ok(());
        };
        match field {
            None => Err(Error::invalid(format!(
                "`{table}` is a table of the module {}, which reads and writes it: it cannot be \
                 dropped while the module is installed. Uninstall the module first; its tables \
                 are kept, and can be dropped afterwards.",
                claim.module
            ))),
            Some(field) if claim.fields.iter().any(|f| f == field) => Err(Error::invalid(format!(
                "`{table}.{field}` is a field the module {} declared, and its code depends \
                     on it: it cannot be dropped while the module is installed. Fields you added \
                     yourself can be.",
                claim.module
            ))),
            Some(_) => Ok(()),
        }
    }

    /// Refuse renaming a field a module declared, for the reason it may not be
    /// dropped: the module's code names it.
    pub fn refuse_rename(&self, table: &str, field: &str) -> Result<()> {
        match self.claims.get(table) {
            Some(claim) if claim.fields.iter().any(|f| f == field) => Err(Error::invalid(format!(
                "`{table}.{field}` is a field the module {} declared, and its code names it: \
                 it cannot be renamed while the module is installed",
                claim.module
            ))),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_table_and_its_declared_fields_cannot_be_dropped_but_added_fields_can() {
        let mut tables = ModuleTables::new();
        assert!(tables.claim(
            "mail_messages",
            "@feldspar/imap",
            vec!["id".into(), "subject".into()]
        ));
        assert_eq!(tables.module_of("mail_messages"), Some("@feldspar/imap"));
        assert!(tables.declares_field("mail_messages", "subject"));
        assert!(!tables.declares_field("mail_messages", "handled"));

        let msg = tables
            .refuse_drop("mail_messages", None)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("@feldspar/imap") && msg.contains("Uninstall"),
            "{msg}"
        );
        let msg = tables
            .refuse_drop("mail_messages", Some("subject"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`mail_messages.subject`"), "{msg}");
        tables
            .refuse_drop("mail_messages", Some("handled"))
            .unwrap();
        tables.refuse_drop("orders", None).unwrap();
    }

    #[test]
    fn the_first_claim_on_a_table_keeps_it() {
        let mut tables = ModuleTables::new();
        assert!(tables.claim("t", "a", vec![]));
        assert!(!tables.claim("t", "b", vec![]));
        assert_eq!(tables.module_of("t"), Some("a"));
    }
}
