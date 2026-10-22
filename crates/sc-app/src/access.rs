//! Who may use an application at all: its `min_role`, `max_role` and
//! `access_if` (TODO F9).
//!
//! Every other rule in the system is about one thing — an endpoint's role
//! floor, a table's ownership formula, a stream's `min_role`. These three are
//! about the **application**, and they sit in front of all of those:
//!
//! - **`min_role`** — a caller whose role number is higher (less privileged)
//!   is not let in at all. A signed-in caller is refused every request, and a
//!   sign-in as them is refused before a session exists.
//! - **`access_if`** — a formula over `user` (and the time, and anything else
//!   a formula may read without a row) that must hold, with the same refusal.
//!   It sees the caller's **real** role: it is a question about the person.
//! - **`max_role`** — a caller who is let in holds at most this role while
//!   they use the application. The cap is applied to the [`User`] itself, so
//!   every rule downstream (endpoint floors, ownership formulas, RLS, code
//!   queries) sees the capped role without knowing a cap exists.
//!
//! A caller nobody is signed in as holds the public role (100) and has no
//! fields, so they are admitted only when `min_role` is 100 and `access_if`
//! holds of `user = null`. One who is not admitted may still reach the
//! application's sign-in surface — the server decides what that is — because
//! an application nobody can sign in to is an application nobody can use.

use std::collections::BTreeMap;

use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, User};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::{Formula, FormulaCall, JsEvaluator, Operation, TableShape};
use sc_query::Value;

use crate::application::Application;

/// The bare scope `access_if` is validated against: a table with no fields,
/// so a bare identifier is refused (there is no row) and `user.x` is checked
/// against the users table.
const NO_ROW: &str = "(application access)";

/// What [`admit`] decided about one caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// Let in, as this caller: `None` for an anonymous one, otherwise the user
    /// with their role capped at the application's `max_role`.
    Admitted(Option<User>),
    /// Not let in. Only the application's sign-in surface is open to them.
    Refused,
}

impl Admission {
    /// The admitted caller, or `None` when refused.
    pub fn admitted(self) -> Option<Option<User>> {
        match self {
            Admission::Admitted(user) => Some(user),
            Admission::Refused => None,
        }
    }
}

impl Application {
    /// Whether the application refuses anybody at all — what lets a request to
    /// an application without these settings skip [`admit`]'s work.
    pub fn is_gated(&self) -> bool {
        self.min_role < ROLE_PUBLIC || !self.access_if.trim().is_empty()
    }

    /// Whether the application caps anybody's role.
    pub fn caps_role(&self) -> bool {
        self.max_role > ROLE_ADMIN
    }
}

/// Decide whether `user` (`None`: nobody is signed in) may use `app`, and as
/// whom.
///
/// `access_if` fails closed: a formula that will not parse, throws, times out,
/// or has no evaluator to run on refuses the caller.
pub async fn admit(
    app: &Application,
    user: Option<User>,
    evaluator: Option<&dyn JsEvaluator>,
) -> Admission {
    let role = user.as_ref().map_or(ROLE_PUBLIC, |u| u.role);
    if role > app.min_role {
        return Admission::Refused;
    }
    let source = app.access_if.trim();
    if !source.is_empty() {
        let Some(evaluator) = evaluator else {
            return Admission::Refused;
        };
        let Ok(formula) = Formula::parse(source) else {
            return Admission::Refused;
        };
        let call = FormulaCall {
            formula,
            op: Operation::Read,
            row: BTreeMap::new(),
            user: user.as_ref().map(user_values),
            ambient: Default::default(),
        };
        if !evaluator.eval(call).await.unwrap_or(false) {
            return Admission::Refused;
        }
    }
    Admission::Admitted(user.map(|u| cap_role(app, u)))
}

/// `user` as `app` serves them: their role, or the application's `max_role`
/// when that is less privileged.
pub fn cap_role(app: &Application, mut user: User) -> User {
    user.role = user.role.max(app.max_role);
    user
}

/// The user object as `access_if` sees it: `id`, `role` and every other field
/// — the shape an ownership formula's `user` has.
fn user_values(user: &User) -> BTreeMap<String, Value> {
    let mut map = user.extra.clone();
    map.insert(sc_auth::COL_ID.to_owned(), Value::Uuid(user.id));
    map.insert(
        sc_auth::COL_ROLE.to_owned(),
        Value::Int(i64::from(user.role)),
    );
    map
}

/// Refuse access settings that cannot mean anything: a role outside
/// `1..=100`, or an `access_if` that does not parse or names something that is
/// not in scope.
///
/// A `max_role` less privileged than `min_role` is allowed and means what it
/// says: only callers at least as privileged as `min_role` get in, and every
/// one of them is served as `max_role`.
pub(crate) fn validate_access(catalog: &Catalog, app: &Application) -> Result<()> {
    validate_roles(app)?;
    let source = app.access_if.trim();
    if source.is_empty() {
        return Ok(());
    }
    let refused = |e: &dyn std::fmt::Display| {
        Error::invalid(format!("application `{}`: `access_if`: {e}", app.name))
    };
    let formula = Formula::parse(source).map_err(|e| refused(&e))?;
    let shape = catalog.schema_shape()?.table(NO_ROW, TableShape::new());
    let analysis = formula.validate(&shape, NO_ROW).map_err(|e| refused(&e))?;
    // Fail closed means a module that is down refuses everybody, and every
    // request would wait on it: the reason an ownership formula may not call
    // one either.
    if let Some(call) = analysis.first_module_call() {
        return Err(refused(&format_args!(
            "it may not call the module function `{}`: it decides who may use the application, \
             so a module that is slow or down would lock everybody out",
            call.function
        )));
    }
    if let Some(call) = analysis.first_model_call() {
        return Err(refused(&format_args!(
            "it may not call `{}`: it decides who may use the application, so a model that \
             cannot answer would lock everybody out",
            call.key
        )));
    }
    Ok(())
}

/// The role half of [`validate_access`].
fn validate_roles(app: &Application) -> Result<()> {
    let range = ROLE_ADMIN..=ROLE_PUBLIC;
    for (name, role) in [("min_role", app.min_role), ("max_role", app.max_role)] {
        if !range.contains(&role) {
            return Err(Error::invalid(format!(
                "application `{}`: `{name}` should be a role number, 1 to 100, got {role}",
                app.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::FrameworkRef;
    use sc_catalog::Attrs;
    use uuid::Uuid;

    fn app() -> Application {
        Application::new(
            "Ledger",
            "ledger",
            FrameworkRef {
                name: "none".into(),
                config: Attrs::new(),
            },
        )
    }

    fn user(role: u8, department: &str) -> User {
        let mut user = User::new(Uuid::new_v4(), role).unwrap();
        user.extra
            .insert("department".into(), Value::Text(department.into()));
        user
    }

    #[tokio::test]
    async fn an_application_with_no_settings_admits_everybody_as_they_are() {
        let app = app();
        assert!(!app.is_gated() && !app.caps_role());
        assert_eq!(admit(&app, None, None).await, Admission::Admitted(None));
        let admin = user(1, "IT");
        assert_eq!(
            admit(&app, Some(admin.clone()), None).await,
            Admission::Admitted(Some(admin))
        );
    }

    #[tokio::test]
    async fn min_role_refuses_less_privileged_callers_and_anonymous_ones() {
        let app = app().min_role(40);
        assert!(app.is_gated());
        assert_eq!(admit(&app, None, None).await, Admission::Refused);
        assert_eq!(
            admit(&app, Some(user(80, "")), None).await,
            Admission::Refused
        );
        let staff = user(40, "");
        assert_eq!(
            admit(&app, Some(staff.clone()), None).await,
            Admission::Admitted(Some(staff))
        );
    }

    #[tokio::test]
    async fn max_role_caps_a_more_privileged_caller_and_leaves_others_alone() {
        let app = app().max_role(20);
        let Admission::Admitted(Some(admin)) = admit(&app, Some(user(1, "")), None).await else {
            panic!("an admin is let in");
        };
        assert_eq!(admin.role, 20, "served as role 20");
        let Admission::Admitted(Some(staff)) = admit(&app, Some(user(40, "")), None).await else {
            panic!("staff are let in");
        };
        assert_eq!(staff.role, 40, "a less privileged role is not raised");
    }

    #[tokio::test]
    async fn access_if_with_no_evaluator_or_a_broken_formula_fails_closed() {
        let app = app().access_if("user.department === \"Finance\"");
        assert_eq!(
            admit(&app, Some(user(1, "Finance")), None).await,
            Admission::Refused
        );
        let broken = app.clone().access_if("user.department ===");
        assert_eq!(
            admit(&broken, Some(user(1, "Finance")), None).await,
            Admission::Refused
        );
    }

    #[test]
    fn roles_out_of_range_are_refused() {
        assert!(validate_roles(&app().min_role(40).max_role(20)).is_ok());
        // Only staff get in, and all of them are served as role 80.
        assert!(validate_roles(&app().min_role(40).max_role(80)).is_ok());
        let zero = validate_roles(&app().min_role(0)).unwrap_err();
        assert!(zero.to_string().contains("min_role"), "{zero}");
        assert!(validate_roles(&app().max_role(0)).is_err());
        assert!(validate_roles(&app().min_role(101)).is_err());
    }
}
