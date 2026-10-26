//! A v1 workflow as this system's workflow (§10.3): the same program, step for
//! step, in the five step kinds this system has.
//!
//! The two engines agree on the shape — named steps, a start, a "what runs next"
//! per step, loops whose body ends by coming back — and differ in one place that
//! matters: **where a run keeps its variables**. v1 merges everything into one
//! flat context and its formulas name variables bare (`orders`, `order.id`);
//! here the context is the ambient `context`, a step's result is kept under the
//! step's name, a loop's item under its variable and a form's answers under the
//! form step. So every v1 formula is rewritten on the way through:
//!
//! - a variable a step of this workflow defines becomes where that step puts it
//!   — `TableQuery`'s `orders` is `context.<the step>`, a `UserForm` question
//!   `title` is `context.<the step>.title`, a loop's `order` is `context.order`;
//! - any other bare name is v1's context too, and becomes `context.<name>` —
//!   except `user` and the JavaScript globals, which mean the same thing in both.
//!
//! The step kinds:
//!
//! | v1 | here |
//! |---|---|
//! | `TableQuery` | an `Action` step running `run_js_code` over `db.table(…)` |
//! | `ForLoop` | `ForEach` |
//! | `SetContext` | `Set` |
//! | `UserForm` | `UserForm`, answers under the step |
//! | `TerminateWorkflow` | an empty `Set` that ends the run |
//! | `modify_row` on a table | `update_rows`, its query as the `where` |
//! | any other action | an `Action` step of that name, as a trigger's action crosses |
//!
//! What has no counterpart — a step's `only_if`, a loop's index variable, v1's
//! locks and error handlers — fails the whole workflow with a reason, rather
//! than producing a program that runs differently from the one that was drawn.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_expr::{object_literal_entries, rewrite_free_refs};
use serde_json::{Map, Value as Json, json};

use super::{array, name_of, text};

/// Names that mean the same thing in a v1 formula and in one here, and so are
/// never read as a context variable.
const AMBIENT: &[&str] = &[
    "user",
    "Math",
    "Date",
    "JSON",
    "Number",
    "String",
    "Boolean",
    "Array",
    "Object",
    "parseInt",
    "parseFloat",
    "isNaN",
    "undefined",
    "NaN",
    "Infinity",
    "context",
];

/// Configuration keys v1 writes for its own canvas, which mean nothing here.
const V1_LAYOUT_KEYS: &[&str] = &["workflow_position"];

/// The workflow document a v1 workflow trigger's `steps` become — what
/// `triggers.json` carries under `workflow` and the restore saves as version 1.
pub(super) fn convert_workflow(trigger: &Map<String, Json>) -> Result<Json> {
    let steps: Vec<Map<String, Json>> = array(trigger, "steps")
        .into_iter()
        .filter_map(|s| s.as_object().cloned())
        .collect();
    if steps.is_empty() {
        return Err(Error::invalid("it has no steps"));
    }
    let names: Vec<String> = steps.iter().map(name_of).collect();
    let start = steps
        .iter()
        .find(|s| s.get("initial_step").and_then(Json::as_bool) == Some(true))
        .map(name_of)
        .unwrap_or_else(|| names[0].clone());
    let scope = Scope::of(&steps);
    let converted = steps
        .iter()
        .map(|step| {
            convert_step(step, &scope, &names)
                .map_err(|e| Error::invalid(format!("step `{}`: {}", name_of(step), e.causes())))
        })
        .collect::<Result<Vec<Json>>>()?;
    Ok(json!({ "start": start, "steps": converted }))
}

/// Where each variable a v1 workflow defines lives here, by its v1 name.
struct Scope {
    aliases: BTreeMap<String, String>,
}

impl Scope {
    fn of(steps: &[Map<String, Json>]) -> Scope {
        let mut aliases = BTreeMap::new();
        for step in steps {
            let name = name_of(step);
            let config = configuration(step);
            match text(step, "action_name").as_str() {
                "TableQuery" => {
                    let var = text(&config, "query_variable");
                    if !var.is_empty() {
                        aliases.insert(var, format!("context.{name}"));
                    }
                }
                "UserForm" => {
                    for question in array(&config, "user_form_questions") {
                        if let Some(var) = question.get("var_name").and_then(Json::as_str)
                            && !var.is_empty()
                        {
                            aliases.insert(var.to_owned(), format!("context.{name}.{var}"));
                        }
                    }
                }
                _ => {}
            }
        }
        Scope { aliases }
    }

    /// A v1 formula over the run's context, as one over `context` here.
    fn formula(&self, source: &str) -> Result<String> {
        self.formula_with(source, &[])
    }

    /// The same, with `literal` names (a workflow's step names, in a v1 `next`
    /// expression) turned into the strings v1 bound them to.
    fn formula_with(&self, source: &str, literal: &[String]) -> Result<String> {
        rewrite_free_refs(source, &mut |path| {
            let head = &path[0];
            if literal.contains(head) {
                return Some((1, json!(head).to_string()));
            }
            if let Some(alias) = self.aliases.get(head) {
                return Some((1, alias.clone()));
            }
            (!AMBIENT.contains(&head.as_str())).then(|| (1, format!("context.{head}")))
        })
    }
}

fn configuration(step: &Map<String, Json>) -> Map<String, Json> {
    let mut config = step
        .get("configuration")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    for key in V1_LAYOUT_KEYS {
        config.remove(*key);
    }
    config
}

fn convert_step(step: &Map<String, Json>, scope: &Scope, names: &[String]) -> Result<Json> {
    let name = name_of(step);
    if !text(step, "only_if").trim().is_empty() {
        return Err(Error::invalid(
            "it has an `only_if`, and a step here has none; draw it as a branch",
        ));
    }
    let config = configuration(step);
    let action = text(step, "action_name");
    let kind = match action.as_str() {
        "TableQuery" => {
            let table = text(&config, "query_table");
            if table.is_empty() {
                return Err(Error::invalid("its query names no table"));
            }
            let query = match text(&config, "query_object").trim() {
                "" => "{}".to_owned(),
                query => scope.formula(query)?,
            };
            json!({ "type": "action", "action": "run_js_code", "configuration": {
                "code": format!(
                    "return await db.table({}).where({query}).rows();",
                    json!(table)
                ),
            } })
        }
        "ForLoop" => {
            if !text(&config, "index_variable").trim().is_empty() {
                return Err(Error::invalid(
                    "its loop has an index variable, which a loop here does not keep",
                ));
            }
            json!({ "type": "for_each",
                "over": scope.formula(&text(&config, "array_expression"))?,
                "var": text(&config, "item_variable"),
                "body": text(&config, "loop_body_initial_step"),
            })
        }
        "SetContext" => {
            let assignments = object_literal_entries(&text(&config, "ctx_values"))?
                .into_iter()
                .map(|(target, value)| {
                    Ok(json!({ "target": target, "formula": scope.formula(&value)? }))
                })
                .collect::<Result<Vec<Json>>>()?;
            json!({ "type": "set", "assignments": assignments })
        }
        "TerminateWorkflow" => json!({ "type": "set", "assignments": [] }),
        "UserForm" => user_form(&name, &config)?,
        "modify_row" => modify_row(&config, scope)?,
        "AcquireLock" | "ReleaseLock" | "SetErrorHandler" | "WaitUntilTime" | "WaitNextTick"
        | "EditViewForm" | "APIResponse" | "Output" => {
            return Err(Error::invalid(format!(
                "`{action}` is a v1 workflow step this system has no counterpart for"
            )));
        }
        "" => return Err(Error::invalid("it names no action")),
        // Any other action, as a trigger's crosses: by name, with its settings,
        // for the workflow's validation to accept or to refuse by that name.
        other => json!({ "type": "action", "action": other, "configuration": config }),
    };
    // A loop ends its run differently here than its body does, but a v1
    // `TerminateWorkflow` ends the run either way.
    let next = if action == "TerminateWorkflow" {
        json!({ "type": "end" })
    } else {
        next_of(&text(step, "next_step"), scope, names)?
    };
    Ok(json!({ "name": name, "kind": kind, "next": next }))
}

/// v1's `next_step` — an expression over the step names, nearly always one of
/// them — as a plain edge, the end, or (for anything else) a formula.
fn next_of(next: &str, scope: &Scope, names: &[String]) -> Result<Json> {
    let next = next.trim();
    if next.is_empty() {
        return Ok(json!({ "type": "end" }));
    }
    if names.iter().any(|n| n == next) {
        return Ok(json!({ "type": "step", "step": next }));
    }
    Ok(json!({ "type": "formula", "formula": scope.formula_with(next, names)? }))
}

/// v1's question types, as the basic types a form here can ask for.
fn user_form(step: &str, config: &Map<String, Json>) -> Result<Json> {
    let mut fields = Vec::new();
    for question in array(config, "user_form_questions") {
        let Some(question) = question.as_object() else {
            continue;
        };
        let var = text(question, "var_name");
        let qtype = text(question, "qtype");
        let mut field = json!({ "name": var, "label": text(question, "label") });
        let type_name = match qtype.as_str() {
            "Yes/No" | "Checkbox" => "bool",
            "Free text" | "Multiple choice" => "text",
            "Integer" => "int",
            "Float" => "float",
            "Date" => "date",
            other => {
                return Err(Error::invalid(format!(
                    "its question `{var}` is a `{other}`, which a form here cannot ask"
                )));
            }
        };
        field["type"] = json!(type_name);
        if qtype == "Multiple choice" {
            field["options"] = json!(
                text(question, "options")
                    .split(',')
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .collect::<Vec<_>>()
            );
        }
        fields.push(field);
    }
    Ok(json!({ "type": "user_form", "fields": fields, "assign_to": step }))
}

/// v1's `modify_row` over a chosen table: the rows its query object selects,
/// given the values its row expression computes — which is `update_rows`, with
/// the query as a `where` over the target row and the context as `context`.
fn modify_row(config: &Map<String, Json>, scope: &Scope) -> Result<Json> {
    let table = text(config, "select_table");
    let query = text(config, "query");
    if table.is_empty() || query.trim().is_empty() {
        return Err(Error::invalid(
            "it modifies the workflow's own row, which a workflow here does not have; \
             only a `modify_row` on a chosen table crosses",
        ));
    }
    let conditions = object_literal_entries(&query)?
        .into_iter()
        .map(|(field, value)| Ok(format!("{field} === ({})", scope.formula(&value)?)))
        .collect::<Result<Vec<String>>>()?;
    let predicate = if conditions.is_empty() {
        "true".to_owned()
    } else {
        conditions.join(" && ")
    };
    let mut assignments = Map::new();
    for (field, value) in object_literal_entries(&text(config, "row_expr"))? {
        assignments.insert(field, json!(scope.formula(&value)?));
    }
    Ok(
        json!({ "type": "action", "action": "update_rows", "configuration": {
        "table": table,
        "where": predicate,
        "assignments": assignments,
    } }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workflow in the backup this was written against: query the orders,
    /// loop over them, discount each, stop.
    fn discount_all() -> Map<String, Json> {
        json!({ "name": "MyWorkflow", "action": "Workflow", "steps": [
            { "name": "step1", "next_step": "step2", "only_if": "", "initial_step": true,
              "action_name": "TableQuery", "configuration": {
                "query_table": "order", "query_object": "{}", "query_variable": "orders" } },
            { "name": "step2", "next_step": "step4", "only_if": "", "initial_step": false,
              "action_name": "ForLoop", "configuration": {
                "item_variable": "order", "index_variable": "", "array_expression": "orders",
                "loop_body_initial_step": "step3" } },
            { "name": "step3", "next_step": "", "only_if": "", "initial_step": false,
              "action_name": "modify_row", "configuration": {
                "query": "{id: order.id}", "where": "", "row_expr": "{discount: 0.15}",
                "select_table": "order", "workflow_position": { "x": 1, "y": 2 } } },
            { "name": "step4", "next_step": "", "only_if": "", "initial_step": false,
              "action_name": "TerminateWorkflow", "configuration": { "return_value": "{}" } },
        ] })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn a_query_a_loop_and_a_row_update_become_the_same_program() {
        let workflow = convert_workflow(&discount_all()).unwrap();
        assert_eq!(workflow["start"], json!("step1"));
        let steps = workflow["steps"].as_array().unwrap();
        assert_eq!(
            steps[0],
            json!({ "name": "step1",
                "kind": { "type": "action", "action": "run_js_code", "configuration": {
                    "code": "return await db.table(\"order\").where({}).rows();" } },
                "next": { "type": "step", "step": "step2" } })
        );
        // The loop reads where the query step put its rows.
        assert_eq!(
            steps[1]["kind"],
            json!({ "type": "for_each", "over": "context.step1", "var": "order", "body": "step3" })
        );
        assert_eq!(steps[1]["next"], json!({ "type": "step", "step": "step4" }));
        assert_eq!(
            steps[2]["kind"],
            json!({ "type": "action", "action": "update_rows", "configuration": {
                "table": "order", "where": "id === (context.order.id)",
                "assignments": { "discount": "0.15" } } })
        );
        // The end of the loop body is the end of an iteration.
        assert_eq!(steps[2]["next"], json!({ "type": "end" }));
        assert_eq!(
            steps[3]["kind"],
            json!({ "type": "set", "assignments": [] })
        );
    }

    #[test]
    fn the_shape_is_one_the_engine_reads() {
        let mut document = convert_workflow(&discount_all()).unwrap();
        document["id"] = json!(uuid::Uuid::new_v4());
        document["version"] = json!(1);
        let workflow: sc_workflow::Workflow = serde_json::from_value(document).unwrap();
        assert_eq!(workflow.steps.len(), 4);
    }

    #[test]
    fn a_form_keeps_its_answers_under_the_step_and_later_steps_read_them_there() {
        let trigger = json!({ "steps": [
            { "name": "ask", "next_step": "keep", "initial_step": true, "action_name": "UserForm",
              "configuration": { "user_form_questions": [
                { "label": "Title", "qtype": "Free text", "var_name": "title" },
                { "label": "Size", "qtype": "Multiple choice", "var_name": "size",
                  "options": "S, M,L" } ] } },
            { "name": "keep", "next_step": "", "action_name": "SetContext",
              "configuration": { "ctx_values": "{ shout: title.toUpperCase(), n: count + 1 }" } },
        ] });
        let workflow = convert_workflow(trigger.as_object().unwrap()).unwrap();
        assert_eq!(
            workflow["steps"][0]["kind"],
            json!({ "type": "user_form", "assign_to": "ask", "fields": [
                { "name": "title", "label": "Title", "type": "text" },
                { "name": "size", "label": "Size", "type": "text", "options": ["S", "M", "L"] } ] })
        );
        assert_eq!(
            workflow["steps"][1]["kind"]["assignments"],
            json!([
                { "target": "shout", "formula": "context.ask.title.toUpperCase()" },
                { "target": "n", "formula": "context.count + 1" },
            ])
        );
    }

    #[test]
    fn a_computed_next_names_the_steps_as_strings() {
        let scope = Scope {
            aliases: BTreeMap::new(),
        };
        let names = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            next_of("n > 2 ? a : b", &scope, &names).unwrap(),
            json!({ "type": "formula", "formula": "context.n > 2 ? \"a\" : \"b\"" })
        );
    }

    #[test]
    fn what_has_no_counterpart_fails_the_workflow_naming_the_step() {
        let mut trigger = Json::Object(discount_all());
        trigger["steps"][1]["only_if"] = json!("orders.length > 0");
        let e = convert_workflow(trigger.as_object().unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("step `step2`") && e.contains("only_if"), "{e}");
    }
}
