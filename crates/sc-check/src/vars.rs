//! Variables (TODO C1.3): a step's `as` captures its value, and a later step's
//! string parameters are `sc-expr` templates over everything captured so far,
//! with `run.id` always in scope.
//!
//! Every string anywhere in a step's parameters is a template — inside a row
//! object as much as at the top level — because the commonest use is a row that
//! names this run (`{"name": "Test customer {{ run.id }}"}`). A string that is
//! **one token and nothing else** keeps the captured value's type, so
//! `{"id": "{{ order.id }}"}` passes a number when the id is one; any other
//! string renders to a string.

use sc_error::{Error, Result};
use sc_expr::Template;
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

/// The name `run` is reserved for: `{{ run.id }}`.
pub const RUN_VAR: &str = "run";

/// The variables a run starts with.
pub fn initial(run_id: Uuid) -> Json {
    json!({ RUN_VAR: { "id": run_id.to_string() } })
}

/// Store a captured value as `name`.
pub fn capture(vars: &mut Json, name: &str, value: Json) {
    if let Json::Object(map) = vars {
        map.insert(name.to_owned(), value);
    }
}

/// `params` with every string rendered over `vars`.
pub fn render(params: &Json, vars: &Json) -> Result<Json> {
    Ok(match params {
        Json::String(s) if s.contains("{{") => {
            let template = Template::parse(s)?;
            match template.sole_path(vars) {
                Some(value) => value?,
                None => Json::String(template.render_paths(vars)?),
            }
        }
        Json::Array(items) => Json::Array(
            items
                .iter()
                .map(|item| render(item, vars))
                .collect::<Result<_>>()?,
        ),
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), render(v, vars)?)))
                .collect::<Result<Map<_, _>>>()?,
        ),
        other => other.clone(),
    })
}

/// Refuse, on save, a parameter that does not parse as a template — so a
/// typo is found while the step is being written rather than in its first run.
pub fn check(params: &Json) -> Result<()> {
    match params {
        Json::String(s) if s.contains("{{") => Template::parse(s).map(|_| ()),
        Json::Array(items) => items.iter().try_for_each(check),
        Json::Object(map) => map.values().try_for_each(check),
        _ => Ok(()),
    }
}

/// Refuse a capture name that is not an identifier, or is `run`.
pub fn check_capture(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let starts = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::invalid(format!(
            "`as: {name}` should be a name a template can use: letters, digits and `_`, \
             not starting with a digit"
        )));
    }
    if name == RUN_VAR {
        return Err(Error::invalid(
            "`as: run` is taken: `{{ run.id }}` is this run's id",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_render_anywhere_and_a_sole_token_keeps_its_type() {
        let run = Uuid::new_v4();
        let mut vars = initial(run);
        capture(&mut vars, "order", json!({ "id": 7, "name": "x" }));
        let params = json!({
            "table": "orders",
            "row": { "name": "Test {{ run.id }}", "parent": "{{ order.id }}" },
            "list": ["{{ order.name }}", 3],
        });
        let out = render(&params, &vars).unwrap();
        assert_eq!(out["row"]["name"], json!(format!("Test {run}")));
        assert_eq!(out["row"]["parent"], json!(7));
        assert_eq!(out["list"], json!(["x", 3]));
        assert_eq!(out["table"], json!("orders"));
        // A name nothing captured is an error naming it.
        let msg = render(&json!("{{ customer.id }}"), &vars)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("customer"), "{msg}");
    }

    #[test]
    fn a_capture_name_is_an_identifier_and_not_run() {
        assert!(check_capture("order_id").is_ok());
        assert!(check_capture("run").is_err());
        assert!(check_capture("1st").is_err());
        assert!(check_capture("a-b").is_err());
        assert!(check(&json!({"x": "{{ broken + }}"})).is_err());
        assert!(check(&json!({"x": "{{ fine }}"})).is_ok());
    }
}
