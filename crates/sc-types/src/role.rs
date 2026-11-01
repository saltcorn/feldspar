//! [`Role`]: a privilege level on the fixed `1..=100` scale (technical design §7.1).
//!
//! A role is stored and sent as a bare integer, and lower means **more** privileged: `1` is
//! admin, `100` is public (not signed in). As a `u8` that inverted order was a footgun —
//! `user.role <= table.min_role` is right, `>=` silently grants the opposite, and a role of `0`
//! passed every check (code-quality review §4.1). This type keeps the wire and storage format
//! (it serializes as the number) but takes the comparison away from the call site:
//!
//! - the field is private and only `1..=100` constructs one, so a role is always in range;
//! - [`meets`](Role::meets) is the one access test — "is this role at least as privileged as
//!   that floor";
//! - there is deliberately **no `Ord` / `PartialOrd`**. With an ordering, the old
//!   `role <= min_role` idiom would keep compiling with whichever meaning the ordering picked;
//!   without one it does not compile, and has to say [`meets`](Role::meets).
//!   [`most_privileged`](Role::most_privileged) / [`least_privileged`](Role::least_privileged)
//!   cover the places that combine two roles.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A role on the `1..=100` scale; see the [module docs](self).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Role(u8);

/// A number that is not a role: outside `1..=100`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleOutOfRange(pub i64);

impl fmt::Display for RoleOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "role {} is not in 1..=100", self.0)
    }
}

impl std::error::Error for RoleOutOfRange {}

impl From<RoleOutOfRange> for sc_error::Error {
    fn from(e: RoleOutOfRange) -> Self {
        sc_error::Error::invalid(e.to_string())
    }
}

impl Role {
    /// The most privileged role: full access.
    pub const ADMIN: Role = Role(1);
    /// The least privileged role: public, not signed in.
    pub const PUBLIC: Role = Role(100);

    /// The role numbered `n`, or `None` outside `1..=100`.
    pub const fn new(n: u8) -> Option<Role> {
        if n >= 1 && n <= 100 {
            Some(Role(n))
        } else {
            None
        }
    }

    /// The role numbered `n`, for a **literal** (a constant, a test fixture).
    ///
    /// # Panics
    ///
    /// When `n` is outside `1..=100` — at compile time in a `const`. Never pass it a number that
    /// came from outside the program; use [`Role::new`] or `Role::try_from` for those.
    pub const fn of(n: u8) -> Role {
        match Role::new(n) {
            Some(role) => role,
            None => panic!("a role is in 1..=100"),
        }
    }

    /// The role's number, for storage and the wire. Not for comparing: use
    /// [`meets`](Role::meets).
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether this role is at least as privileged as `floor` — the access test for a
    /// `min_role`.
    pub const fn meets(self, floor: Role) -> bool {
        self.0 <= floor.0
    }

    /// Whether this is [`Role::ADMIN`].
    pub const fn is_admin(self) -> bool {
        self.0 == Role::ADMIN.0
    }

    /// Whether this is [`Role::PUBLIC`].
    pub const fn is_public(self) -> bool {
        self.0 == Role::PUBLIC.0
    }

    /// Whether this role is strictly more privileged than `other`.
    pub const fn outranks(self, other: Role) -> bool {
        self.0 < other.0
    }

    /// The more privileged of two roles.
    pub const fn most_privileged(self, other: Role) -> Role {
        if self.0 <= other.0 { self } else { other }
    }

    /// The less privileged of two roles.
    pub const fn least_privileged(self, other: Role) -> Role {
        if self.0 >= other.0 { self } else { other }
    }
}

/// Least privilege: [`Role::PUBLIC`].
impl Default for Role {
    fn default() -> Self {
        Role::PUBLIC
    }
}

impl fmt::Debug for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Role({})", self.0)
    }
}

/// The number, as an admin sees it (width and alignment flags apply, as for the number).
impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl TryFrom<u8> for Role {
    type Error = RoleOutOfRange;
    fn try_from(n: u8) -> Result<Role, RoleOutOfRange> {
        Role::new(n).ok_or(RoleOutOfRange(i64::from(n)))
    }
}

impl TryFrom<i64> for Role {
    type Error = RoleOutOfRange;
    fn try_from(n: i64) -> Result<Role, RoleOutOfRange> {
        u8::try_from(n)
            .ok()
            .and_then(Role::new)
            .ok_or(RoleOutOfRange(n))
    }
}

impl TryFrom<u64> for Role {
    type Error = RoleOutOfRange;
    fn try_from(n: u64) -> Result<Role, RoleOutOfRange> {
        u8::try_from(n)
            .ok()
            .and_then(Role::new)
            .ok_or(RoleOutOfRange(i64::try_from(n).unwrap_or(i64::MAX)))
    }
}

impl From<Role> for u8 {
    fn from(role: Role) -> u8 {
        role.0
    }
}

impl From<Role> for i64 {
    fn from(role: Role) -> i64 {
        i64::from(role.0)
    }
}

impl From<Role> for serde_json::Value {
    fn from(role: Role) -> serde_json::Value {
        serde_json::Value::from(role.0)
    }
}

impl From<Role> for sc_query::Value {
    fn from(role: Role) -> sc_query::Value {
        sc_query::Value::Int(i64::from(role.0))
    }
}

impl Role {
    /// A role read from a stored or sent value: an integer in `1..=100` (as a number or a
    /// numeric string). `None` for anything else.
    pub fn from_json(value: &serde_json::Value) -> Option<Role> {
        match value {
            serde_json::Value::Number(n) => n.as_i64().and_then(|n| Role::try_from(n).ok()),
            serde_json::Value::String(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| Role::try_from(n).ok()),
            _ => None,
        }
    }

    /// A role read from a row's value: an integer in `1..=100`. `None` for anything else.
    pub fn from_value(value: &sc_query::Value) -> Option<Role> {
        match value {
            sc_query::Value::Int(n) => Role::try_from(*n).ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_to_a_hundred_is_a_role() {
        assert_eq!(Role::new(0), None);
        assert_eq!(Role::new(101), None);
        assert_eq!(Role::new(1), Some(Role::ADMIN));
        assert_eq!(Role::new(100), Some(Role::PUBLIC));
        assert!(Role::try_from(-1i64).is_err());
        assert!(Role::try_from(256i64).is_err());
        assert_eq!(Role::try_from(40i64).unwrap().get(), 40);
    }

    #[test]
    fn meets_reads_lower_numbers_as_more_privilege() {
        let editor = Role::of(40);
        let user = Role::of(80);
        assert!(Role::ADMIN.meets(editor));
        assert!(editor.meets(editor));
        assert!(editor.meets(user));
        assert!(!user.meets(editor));
        assert!(!Role::PUBLIC.meets(user));
        assert!(Role::PUBLIC.meets(Role::PUBLIC));
        assert!(editor.outranks(user));
        assert!(!editor.outranks(editor));
        assert_eq!(editor.most_privileged(user), editor);
        assert_eq!(user.most_privileged(editor), editor);
        assert_eq!(editor.least_privileged(user), user);
        assert!(Role::ADMIN.is_admin() && !editor.is_admin());
        assert!(Role::PUBLIC.is_public());
        assert_eq!(Role::default(), Role::PUBLIC);
        assert_eq!(format!("{:>3}|{}", Role::of(40), Role::ADMIN), " 40|1");
    }

    #[test]
    fn it_serializes_as_the_number_and_refuses_one_out_of_range() {
        assert_eq!(serde_json::to_string(&Role::of(40)).unwrap(), "40");
        assert_eq!(serde_json::from_str::<Role>("40").unwrap(), Role::of(40));
        assert!(serde_json::from_str::<Role>("0").is_err());
        assert!(serde_json::from_str::<Role>("101").is_err());
        assert!(serde_json::from_str::<Role>("\"40\"").is_err());
        assert_eq!(
            Role::from_json(&serde_json::json!("40")),
            Some(Role::of(40))
        );
        assert_eq!(Role::from_json(&serde_json::json!(0)), None);
        assert_eq!(
            Role::from_value(&sc_query::Value::Int(1)),
            Some(Role::ADMIN)
        );
        assert_eq!(
            sc_query::Value::from(Role::PUBLIC),
            sc_query::Value::Int(100)
        );
    }
}
