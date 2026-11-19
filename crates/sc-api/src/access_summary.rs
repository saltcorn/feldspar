//! A table's access rules, said in words (§7.3).
//!
//! The rule is *allowed = the caller meets the operation's floor **or** the
//! ownership formula holds*. A formula adds rows for the roles below the floor;
//! it never limits a role that meets it. That is easy to get backwards — a
//! coding agent asked for "users see only their own rows" sets
//! `min_role_read: 80` and `owner === user.id`, and every signed-in user reads
//! every row. Nothing is wrong with such a table as far as the schema can tell,
//! so this is not a warning or a refusal: it is the rules restated with the
//! deployment's own role names, after `edit_schema` changes them and in
//! `describe_schema`, so that the reader compares what the rules *do* with what
//! was asked for.
//!
//! The formula itself is quoted, never paraphrased. What is worked out about it
//! is only what folds to a constant: the operation flags (`_read && …` grants
//! no writes) and, for a visitor who is not signed in, a comparison with
//! `user.…` (which grants such a visitor nothing, as the translator has it).

use sc_catalog::Table;
use sc_expr::{Ast, BinaryOp, Formula, Operation, UnaryOp};
use sc_types::Role;

/// A role as the summary names it: its number and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleName {
    /// The number on the `1..=100` scale.
    pub role: Role,
    /// Its name, as the admin UI shows it.
    pub name: String,
}

impl RoleName {
    /// A role and its name.
    pub fn new(role: Role, name: impl Into<String>) -> RoleName {
        RoleName {
            role,
            name: name.into(),
        }
    }
}

/// The deployment's roles, most privileged first, for [`summarise`].
///
/// Best effort: the summary must never fail a batch that has been applied, so
/// a catalog with no roles table (or one that cannot be read) gives none, and
/// [`summarise`] then names the two built-in roles.
pub async fn load_roles(catalog: &sc_catalog::Catalog) -> Vec<RoleName> {
    if !matches!(catalog.get(sc_auth::ROLES_TABLE), Ok(Some(_))) {
        return Vec::new();
    }
    match sc_auth::list_roles(catalog).await {
        Ok(roles) => roles
            .into_iter()
            .map(|r| RoleName::new(r.role, r.name))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// What `table`'s access rules mean, in words, with `roles` (most privileged
/// first) named.
pub fn summarise(table: &Table, roles: &[RoleName]) -> String {
    // The two roles every deployment has, should there be no rows to name.
    let builtin = [
        RoleName::new(Role::ADMIN, "admin"),
        RoleName::new(Role::PUBLIC, "public"),
    ];
    let roles = match roles.is_empty() {
        true => &builtin[..],
        false => roles,
    };
    let formula = table.ownership.as_ref();
    let name = &table.name;
    let mut lines = vec![format!("Access to `{name}` as it now stands:")];

    let read = Rule::new(
        table.access.min_role_read,
        roles,
        formula,
        &[Operation::Read],
    );
    lines.push(format!(
        "- Read: {}",
        read.describe("read", "min_role_read")
    ));
    let write = Rule::new(
        table.access.min_role_write,
        roles,
        formula,
        &[Operation::Insert, Operation::Update, Operation::Delete],
    );
    lines.push(format!(
        "- Write (insert, update, delete): {}",
        write.describe("write", "min_role_write")
    ));

    if table.ownership.is_none()
        && let Some(error) = &table.ownership_error
    {
        lines.push(format!(
            "- The stored ownership formula is not in force, so it grants nothing: {error}"
        ));
    }
    // `users` reads with a built-in rule ORed in (`Table::read_rule`).
    if table.read_rule().as_deref() != formula {
        lines.push(format!(
            "- Every signed-in user can also read their own row of `{name}` (a built-in rule)."
        ));
    }

    if let Some(formula) = formula {
        let idle: Vec<&str> = [(&read, "reads"), (&write, "writes")]
            .into_iter()
            .filter(|(rule, _)| rule.formula_idle())
            .map(|(_, what)| what)
            .collect();
        if !idle.is_empty() {
            lines.push(format!(
                "- So the ownership formula `{}` changes nobody's {}. An ownership formula \
                 only adds rows for the roles that do not meet the floor; it never limits a \
                 role that does. To limit signed-in users to the rows it allows, set the \
                 floor to a role more privileged than theirs (`1` leaves only the admin \
                 unrestricted).",
                formula.source(),
                idle.join(" or ")
            ));
        }
    }

    if table.rls_enabled {
        lines.push(
            "- Row-level security is on: the database enforces these rules, and from now \
             on only a person in the admin UI can change them."
                .to_owned(),
        );
    }
    lines.push("Check that this is what was asked for.".to_owned());
    lines.join("\n")
}

/// One operation's rule: the roles that meet its floor, the rest, and what the
/// formula can grant the rest.
struct Rule<'a> {
    floor: Role,
    above: Vec<&'a RoleName>,
    below: Vec<&'a RoleName>,
    formula: Option<&'a Formula>,
    /// The operations the formula may grant a signed-in caller (`Unknown` or
    /// true once the flags are folded).
    signed_in: Vec<Operation>,
    /// Whether it may grant a visitor who is not signed in anything at all.
    anonymous: bool,
}

impl<'a> Rule<'a> {
    fn new(
        floor: Role,
        roles: &'a [RoleName],
        formula: Option<&'a Formula>,
        ops: &[Operation],
    ) -> Rule<'a> {
        let (above, below) = roles.iter().partition(|r| r.role.meets(floor));
        let may = |op: Operation, anonymous: bool| {
            formula.is_some_and(|f| fold(f.ast(), op, anonymous) != Tri::False)
        };
        Rule {
            floor,
            above,
            below,
            formula,
            signed_in: ops.iter().copied().filter(|op| may(*op, false)).collect(),
            anonymous: ops.iter().any(|op| may(*op, true)),
        }
    }

    /// Whether the formula would grant these operations but nobody it could
    /// grant them to is below the floor: every role meets it, or only public
    /// does not and the formula needs a signed-in user. A formula whose flags
    /// leave the operations out (`_read && …` for writes) is not idle but
    /// written that way, and is not said to be.
    fn formula_idle(&self) -> bool {
        if self.formula.is_none() || (self.signed_in.is_empty() && !self.anonymous) {
            return false;
        }
        let signed_in_below = self.below.iter().any(|r| !r.role.is_public());
        let public_below = self.below.iter().any(|r| r.role.is_public());
        !((signed_in_below && !self.signed_in.is_empty()) || (public_below && self.anonymous))
    }

    fn describe(&self, verb: &str, setting: &str) -> String {
        let floor = self.floor;
        if self.below.is_empty() {
            return format!(
                "every role, public visitors who are not signed in included, can {verb} \
                 every row (`{setting}` is {floor})."
            );
        }
        let mut out = match self.above.is_empty() {
            true => format!("no role meets `{setting}` {floor}. "),
            false => format!(
                "{} can {verb} every row (`{setting}` is {floor}{}). ",
                names(&self.above),
                match self.formula {
                    Some(_) => ", and the ownership formula does not restrict a role that meets it",
                    None => "",
                }
            ),
        };
        let below = names(&self.below);
        let Some(formula) = self.formula else {
            out.push_str(&format!("{below} cannot {verb} any row."));
            return out;
        };
        let source = formula.source();
        let all_ops = match verb {
            "read" => 1,
            _ => 3,
        };
        match self.signed_in.len() {
            0 => out.push_str(&format!(
                "{below} cannot {verb} any row: the ownership formula `{source}` grants no \
                 {verb}s."
            )),
            n if n == all_ops => out.push_str(&format!(
                "{below} can {verb} only the rows where `{source}` holds."
            )),
            _ => {
                let ops: Vec<&str> = self.signed_in.iter().map(|op| op_name(*op)).collect();
                out.push_str(&format!(
                    "{below} can only {} the rows where `{source}` holds, and nothing else.",
                    ops.join(" or ")
                ));
            }
        }
        if !self.signed_in.is_empty()
            && self.below.iter().any(|r| r.role.is_public())
            && !self.anonymous
        {
            out.push_str(
                " For public, that is no rows: a visitor who is not signed in has no `user`.",
            );
        }
        out
    }
}

fn op_name(op: Operation) -> &'static str {
    match op {
        Operation::Read => "read",
        Operation::Insert => "insert",
        Operation::Update => "update",
        Operation::Delete => "delete",
    }
}

/// `admin (1), staff (40) and user (80)`.
fn names(roles: &[&RoleName]) -> String {
    let named: Vec<String> = roles
        .iter()
        .map(|r| format!("{} ({})", r.name, r.role))
        .collect();
    match named.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// A formula's value as far as it folds: true, false, or not known without a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

/// Fold `ast` for `op`, for a signed-in caller or (`anonymous`) a visitor with
/// no `user`. Only the operation flags, literals and the logic over them fold,
/// plus — anonymously — `user` itself and a comparison with `user.…`, which
/// the translator makes false ("an anonymous caller owns nothing"), its
/// negation true. Everything else is `Unknown`, so an answer of `False` means
/// the formula grants nothing in any row.
fn fold(ast: &Ast, op: Operation, anonymous: bool) -> Tri {
    let known = |b: bool| if b { Tri::True } else { Tri::False };
    match ast {
        Ast::Bool(b) => known(*b),
        Ast::Ident(name) => match name.as_str() {
            "_read" => known(op == Operation::Read),
            "_insert" => known(op == Operation::Insert),
            "_update" => known(op == Operation::Update),
            "_delete" => known(op == Operation::Delete),
            "_write" => known(op != Operation::Read),
            "user" if anonymous => Tri::False,
            _ => Tri::Unknown,
        },
        Ast::Unary {
            op: UnaryOp::Not,
            expr,
        } => fold(expr, op, anonymous).not(),
        Ast::Binary { op: bin, l, r } => match bin {
            BinaryOp::And => match fold(l, op, anonymous) {
                Tri::False => Tri::False,
                Tri::True => fold(r, op, anonymous),
                Tri::Unknown => match fold(r, op, anonymous) {
                    Tri::False => Tri::False,
                    _ => Tri::Unknown,
                },
            },
            BinaryOp::Or => match fold(l, op, anonymous) {
                Tri::True => Tri::True,
                Tri::False => fold(r, op, anonymous),
                Tri::Unknown => match fold(r, op, anonymous) {
                    Tri::True => Tri::True,
                    _ => Tri::Unknown,
                },
            },
            BinaryOp::Eq | BinaryOp::StrictEq | BinaryOp::NotEq | BinaryOp::StrictNotEq
                if anonymous
                    && (on_user(l) && !matches!(**r, Ast::Null)
                        || on_user(r) && !matches!(**l, Ast::Null)) =>
            {
                known(matches!(bin, BinaryOp::NotEq | BinaryOp::StrictNotEq))
            }
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
                if anonymous && (on_user(l) || on_user(r)) =>
            {
                Tri::False
            }
            _ => Tri::Unknown,
        },
        Ast::Cond { test, cons, alt } => match fold(test, op, anonymous) {
            Tri::True => fold(cons, op, anonymous),
            Tri::False => fold(alt, op, anonymous),
            Tri::Unknown => match (fold(cons, op, anonymous), fold(alt, op, anonymous)) {
                (a, b) if a == b => a,
                _ => Tri::Unknown,
            },
        },
        _ => Tri::Unknown,
    }
}

/// Whether `ast` is a property of `user` (`user.id`, `user.profile.team`).
fn on_user(ast: &Ast) -> bool {
    match ast {
        Ast::Member { obj, .. } => match &**obj {
            Ast::Ident(name) => name == "user",
            inner => on_user(inner),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> Vec<RoleName> {
        vec![
            RoleName::new(Role::ADMIN, "admin"),
            RoleName::new(Role::of(40), "staff"),
            RoleName::new(Role::of(80), "user"),
            RoleName::new(Role::PUBLIC, "public"),
        ]
    }

    fn table(read: u8, write: u8, formula: Option<&str>) -> Table {
        let mut table =
            Table::projected(sc_catalog::DbId::primary(), "notes", Vec::new(), Vec::new());
        table.access.min_role_read = Role::of(read);
        table.access.min_role_write = Role::of(write);
        table.ownership = formula.map(|f| Formula::parse(f).unwrap());
        table
    }

    fn fold_src(src: &str, op: Operation, anonymous: bool) -> Tri {
        fold(Formula::parse(src).unwrap().ast(), op, anonymous)
    }

    #[test]
    fn a_formula_under_a_floor_every_signed_in_role_meets_is_said_to_change_nothing() {
        // The mistake this module exists for: "users see only their own rows"
        // written as floor 80 plus an ownership formula.
        let text = summarise(&table(80, 80, Some("owner === user.id")), &roles());
        assert!(
            text.contains("- Read: admin (1), staff (40) and user (80) can read every row"),
            "{text}"
        );
        assert!(
            text.contains("For public, that is no rows: a visitor who is not signed in"),
            "{text}"
        );
        assert!(
            text.contains("changes nobody's reads or writes. An ownership formula only adds"),
            "{text}"
        );
        assert!(
            text.ends_with("Check that this is what was asked for."),
            "{text}"
        );
    }

    #[test]
    fn a_formula_below_a_staff_floor_is_described_and_not_called_idle() {
        let text = summarise(&table(40, 40, Some("owner === user.id")), &roles());
        assert!(
            text.contains("admin (1) and staff (40) can read every row (`min_role_read` is 40"),
            "{text}"
        );
        assert!(
            text.contains(
                "user (80) and public (100) can read only the rows where `owner === user.id` holds."
            ),
            "{text}"
        );
        assert!(!text.contains("changes nobody's"), "{text}");
        assert!(!text.contains("row-level security"), "{text}");
    }

    #[test]
    fn the_operation_flags_say_which_operations_the_formula_grants() {
        let text = summarise(
            &table(1, 1, Some("_read && owner === user.id || _insert")),
            &roles(),
        );
        assert!(text.contains("can read only the rows where"), "{text}");
        assert!(text.contains("can only insert the rows where"), "{text}");
        let text = summarise(&table(1, 1, Some("_read && owner === user.id")), &roles());
        assert!(
            text.contains("cannot write any row: the ownership formula `_read && owner === user.id` grants no writes"),
            "{text}"
        );
        // Leaving writes out is how the formula was written, not a mistake.
        assert!(!text.contains("changes nobody's"), "{text}");
        assert!(text.contains("grants no writes.\nCheck"), "{text}");
    }

    #[test]
    fn no_formula_and_a_public_floor_are_plain_role_rules() {
        let text = summarise(&table(100, 1, None), &roles());
        assert!(
            text.contains("- Read: every role, public visitors who are not signed in included"),
            "{text}"
        );
        assert!(
            text.contains("staff (40), user (80) and public (100) cannot write any row."),
            "{text}"
        );
        assert!(!text.contains("ownership formula"), "{text}");
    }

    #[test]
    fn row_level_security_is_said_to_be_a_persons_from_now_on() {
        let mut t = table(1, 1, Some("owner === user.id"));
        t.rls_enabled = true;
        assert!(summarise(&t, &roles()).contains("only a person in the admin UI"));
    }

    #[test]
    fn folding_is_constant_only_where_the_answer_holds_in_every_row() {
        use Operation::*;
        assert_eq!(fold_src("_read && x === 1", Insert, false), Tri::False);
        assert_eq!(fold_src("_read && x === 1", Read, false), Tri::Unknown);
        assert_eq!(fold_src("_write || x === 1", Delete, false), Tri::True);
        assert_eq!(fold_src("owner === user.id", Read, true), Tri::False);
        assert_eq!(fold_src("owner !== user.id", Read, true), Tri::True);
        // A comparison with literal null is about the field, not the caller.
        assert_eq!(fold_src("user.id === null", Read, true), Tri::Unknown);
        assert_eq!(fold_src("user && user.role <= 40", Read, true), Tri::False);
        assert_eq!(fold_src("public === true", Read, true), Tri::Unknown);
        assert_eq!(fold_src("!_read", Read, false), Tri::False);
    }
}
