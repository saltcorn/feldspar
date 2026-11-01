//! A refusal from the row layer, as the GraphQL error a client can act on.
//!
//! Everywhere else in this provider an [`Error`] reaches the wire through
//! `async-graphql`'s blanket `From<T: Display>`, which keeps the message and
//! throws away everything else. That is enough for a read, whose failures are a
//! developer's to fix while writing the query. It is not enough for a **write**:
//! a mutation is run by an application on behalf of a person filling in a form,
//! and that application has to tell "this row is not yours" from "the server
//! fell over" *without parsing English*.
//!
//! So a mutation's error carries `extensions`:
//!
//! - **`code`** — the class of refusal, from the [`Repr`] the row layer chose.
//!   The vocabulary is the conventional GraphQL one (`BAD_USER_INPUT`,
//!   `FORBIDDEN`, `NOT_FOUND`, `INTERNAL_SERVER_ERROR`) rather than one invented
//!   here, because a client's error handling is written against that vocabulary
//!   long before it meets Saltcorn.
//! - **`table`** — which table refused. A mutation names it in the field, but a
//!   nested refusal (a `File` field's store, a foreign key) need not.
//! - **`field`** — which column a validation failure was about, when it was
//!   about one. The row layer already writes `` `age`: must be at most 120 ``
//!   because that message is shown to a *user* of an application; this lifts the
//!   same name out of the prose so a form can put it on the right input.
//!
//! The message is the row layer's own, unchanged. One statement of what went
//! wrong, in one place — this module only labels it.

use async_graphql::{ErrorExtensionValues, Value as GqlValue};
use sc_catalog::Table;
use sc_error::{Error, Repr};

/// `code` for a value the caller got wrong — a failed coercion, a rich type's
/// attribute rule, an unknown column, a `File` path outside its store.
const BAD_USER_INPUT: &str = "BAD_USER_INPUT";
/// `code` for a row this caller may not write, or a rule that withheld it.
const FORBIDDEN: &str = "FORBIDDEN";
/// `code` for a row that is not there — which is *also* what a row the caller
/// may not reach looks like, deliberately (§7.3): a mutation must not become a
/// way to probe which rows exist.
const NOT_FOUND: &str = "NOT_FOUND";
/// `code` for a misconfigured application: the admin has something to fix.
const CONFIGURATION_ERROR: &str = "CONFIGURATION_ERROR";
/// `code` for everything else — a driver failure, a broken invariant. Something
/// to report, not something the caller can fix by asking differently.
const INTERNAL_SERVER_ERROR: &str = "INTERNAL_SERVER_ERROR";

/// One row-layer [`Error`] as a GraphQL error against `table`.
pub fn mutation_error(table: &Table, err: Error) -> async_graphql::Error {
    let field = invalid_field(table, &err);
    let mut out = table_error(&table.name, err);
    if let Some((extensions, field)) = out.extensions.as_mut().zip(field) {
        extensions.set("field", GqlValue::String(field));
    }
    out
}

/// The same, for a refusal raised before the table itself could be resolved —
/// where there are no columns to name a `field` against.
pub fn table_error(table: &str, err: Error) -> async_graphql::Error {
    let mut extensions = ErrorExtensionValues::default();
    extensions.set("code", GqlValue::String(code_of(&err).to_owned()));
    extensions.set("table", GqlValue::String(table.to_owned()));
    async_graphql::Error {
        message: err.to_string(),
        source: None,
        extensions: Some(extensions),
    }
}

/// The `code` one error carries.
///
/// The five request-level [`Repr`]s decide it; everything else is a fault rather
/// than an answer, and says so. A wrapped error inherits the code of what it
/// wraps, exactly as [`Error::kind`] inherits its audience — the context a
/// caller added ("while writing the row") does not change what went wrong.
fn code_of(err: &Error) -> &'static str {
    match err.repr() {
        Repr::Invalid(_) | Repr::Query(_) => BAD_USER_INPUT,
        Repr::Auth(_) => FORBIDDEN,
        Repr::NotFound(_) => NOT_FOUND,
        Repr::Config(_) => CONFIGURATION_ERROR,
        Repr::Database(_) | Repr::File(_) | Repr::Serde(_) | Repr::Internal(_) => {
            INTERNAL_SERVER_ERROR
        }
        Repr::Context { source, .. } => source
            .downcast_ref::<Error>()
            .map_or(INTERNAL_SERVER_ERROR, code_of),
        // `Repr` is `non_exhaustive`: a variant added later is a fault to
        // investigate until somebody decides otherwise, which is the same way
        // `Error::kind` treats what it cannot classify.
        _ => INTERNAL_SERVER_ERROR,
    }
}

/// The column a validation failure was about, if it was about one.
///
/// The row layer's `field_error` writes `` `column`: what is wrong with it ``
/// precisely so the message lands a person on the input to fix, and this reads
/// that prefix back out. It is only believed when the name it finds is really a
/// column of the table — a message that merely *starts* with a quoted word is
/// not a field error, and inventing a `field` extension for it would point a
/// form at an input that does not exist.
fn invalid_field(table: &Table, err: &Error) -> Option<String> {
    let message = match err.repr() {
        Repr::Invalid(message) => message.as_str(),
        _ => return None,
    };
    let name = message.strip_prefix('`')?.split_once("`:")?.0;
    table.field(name).map(|_| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, plain_field, table_of};

    fn tasks() -> Table {
        table_of("tasks", vec![id_field(), plain_field("title")])
    }

    /// One extension, as the string it was set to.
    fn extension(err: &async_graphql::Error, key: &str) -> Option<String> {
        match err.extensions.as_ref()?.get(key)? {
            async_graphql::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }

    #[test]
    fn every_class_of_refusal_gets_its_own_code() {
        let table = tasks();
        for (err, expected) in [
            (Error::invalid("nope"), BAD_USER_INPUT),
            (Error::auth("nope"), FORBIDDEN),
            (Error::not_found("nope"), NOT_FOUND),
            (Error::config("nope"), CONFIGURATION_ERROR),
            (Error::database("nope"), INTERNAL_SERVER_ERROR),
        ] {
            let gql = mutation_error(&table, err);
            assert_eq!(extension(&gql, "code").as_deref(), Some(expected));
            assert_eq!(extension(&gql, "table").as_deref(), Some("tasks"));
        }
    }

    #[test]
    fn a_validation_failure_names_the_field_it_was_about() {
        // The row layer's own message shape, which is what a form needs to put
        // the complaint on the right input.
        let err =
            sc_access::rows::column_value(&tasks(), "id", &serde_json::json!("seven")).unwrap_err();
        let gql = mutation_error(&tasks(), err);
        assert_eq!(extension(&gql, "code").as_deref(), Some(BAD_USER_INPUT));
        assert_eq!(extension(&gql, "field").as_deref(), Some("id"));
    }

    #[test]
    fn a_refusal_that_is_not_about_a_field_claims_none() {
        // `field` pointing at an input that does not exist would be worse than
        // no `field` at all.
        let gql = mutation_error(&tasks(), Error::invalid("`elsewhere`: not a column here"));
        assert_eq!(extension(&gql, "field"), None);
        let gql = mutation_error(&tasks(), Error::auth("this row is not yours"));
        assert_eq!(extension(&gql, "field"), None);
    }

    #[test]
    fn a_wrapped_error_keeps_the_code_of_what_it_wraps() {
        use sc_error::Context;
        let err = Err::<(), Error>(Error::auth("not yours"))
            .context("while writing the row")
            .unwrap_err();
        let gql = mutation_error(&tasks(), err);
        assert_eq!(extension(&gql, "code").as_deref(), Some(FORBIDDEN));
    }
}
