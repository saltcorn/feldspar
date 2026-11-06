//! Who changes what only a person may change (TODO-type-hardening H8.5,
//! H8.10; TECHNICAL_DESIGN §7.3a).
//!
//! On a table with row-level security, only a **person** changes who may
//! access it: an administrator in a browser session on the admin host, or an
//! operator with the CLI on the server. Coding agents, MCP clients,
//! `admin_copilot` and code bodies act for an administrator too, but none of
//! them is the administrator looking at the change, so they may turn RLS on
//! and may not, once it is on, change the table's floors, its ownership formula
//! or RLS itself, stricter or looser.
//!
//! That is a type rather than a check each path remembers:
//!
//! - [`Editor`] is what a schema edit (and an approval of a trigger to run as
//!   system) is made by: a [`Person`], an [`Operator`], or
//!   [`Delegated`](Editor::Delegated).
//! - [`RlsTableAccess`] is what changing an RLS table's access settings,
//!   disabling RLS or dropping an RLS table requires, and
//!   [`Editor::rls_table_access`] gives one only to a person or an operator. A
//!   new agent-facing path cannot get round the rule by forgetting a check: it
//!   has no proof to build an editor that would pass.
//!
//! The two proofs are made only by named constructors, each listed in the
//! system-authority inventory (docs/SECURITY_REVIEW_2026-10.md):
//! [`Person::in_admin_session`] (sc-server's `AdminSession`, nothing else) and
//! [`Operator::at_cli`] (the `feldspar` CLI).

use uuid::Uuid;

/// An administrator in a **browser session** on the admin host — not an MCP
/// token, not an agent run, not `Admin::delegated`. Made only by sc-server's
/// `AdminSession`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Person {
    user: Uuid,
    by: &'static str,
}

impl Person {
    /// The administrator `user`, signed in in a browser session the check named
    /// `by` stands behind.
    pub fn in_admin_session(user: Uuid, by: &'static str) -> Person {
        Person { user, by }
    }

    /// The administrator's user id.
    pub fn user_id(&self) -> Uuid {
        self.user
    }

    /// The check that stands behind it.
    pub fn vouched_by(&self) -> &'static str {
        self.by
    }
}

/// The **operator** at the server's command line, doing `what` (`"restore"`).
/// Made only by the `feldspar` CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operator {
    what: &'static str,
}

impl Operator {
    /// The CLI, doing `what`.
    pub const fn at_cli(what: &'static str) -> Operator {
        Operator { what }
    }

    /// What the operator is doing.
    pub fn doing(&self) -> &'static str {
        self.what
    }
}

/// Who makes a schema edit, or approves a trigger to run as system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    /// A person, in the admin UI.
    Person(Person),
    /// The operator, at the CLI.
    Operator(Operator),
    /// Anything else acting for an administrator — a coding agent, an MCP
    /// client, `admin_copilot`, a code body, an admin endpoint called by an
    /// in-process tool layer — named for the log and the refusal.
    Delegated(&'static str),
}

impl Default for Editor {
    /// Delegated: the editor that may do least, so a path that names nobody
    /// cannot change an RLS table's access rules by omission.
    fn default() -> Editor {
        Editor::Delegated("a caller that named no editor")
    }
}

impl Editor {
    /// The proof that changing an RLS table's access rules is allowed: a
    /// person's or an operator's, and `None` for anything delegated.
    pub fn rls_table_access(&self) -> Option<RlsTableAccess> {
        match self {
            Editor::Person(_) | Editor::Operator(_) => Some(RlsTableAccess { _sealed: () }),
            Editor::Delegated(_) => None,
        }
    }

    /// Whether this is a person or the operator.
    pub fn is_person(&self) -> bool {
        self.rls_table_access().is_some()
    }

    /// The person's user id, for an editor that is one.
    pub fn person_id(&self) -> Option<Uuid> {
        match self {
            Editor::Person(person) => Some(person.user_id()),
            Editor::Operator(_) | Editor::Delegated(_) => None,
        }
    }
}

/// Permission to change the access settings of a table with row-level security,
/// to disable RLS, or to drop such a table (§7.3a). Only
/// [`Editor::rls_table_access`] makes one, and only for a person or an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RlsTableAccess {
    _sealed: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_person_or_the_operator_may_change_an_rls_tables_access() {
        let person = Editor::Person(Person::in_admin_session(Uuid::nil(), "a test's session"));
        let operator = Editor::Operator(Operator::at_cli("a test"));
        assert!(person.rls_table_access().is_some());
        assert!(operator.rls_table_access().is_some());
        assert!(
            Editor::Delegated("an MCP client")
                .rls_table_access()
                .is_none()
        );
        assert!(Editor::default().rls_table_access().is_none());
        assert_eq!(person.person_id(), Some(Uuid::nil()));
    }
}
