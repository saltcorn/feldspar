//! v1's built-in actions as this system's (TODO F10.9).
//!
//! v1 has thirty-one actions in its base plugin and this system has a
//! handful, under partly the same names and with different settings: v1's
//! `delete_rows` takes a query object and a "delete the triggering row" box
//! where this one takes a formula, v1's `send_email` renders a view where this
//! one renders templates, and v1's `run_js_code` puts the row's fields in scope
//! by name where this one binds `row`. So no v1 action crosses under its own
//! name with its own settings. Each one is **translated** — into the action
//! here that does the same thing, or, far more often, into a `run_js_code` body
//! that does what v1's `run` did, written against the v1 `Table` this system's
//! code bodies already have — or it is **refused with the reason**, where this
//! system has nothing it could be translated into (a notification, a custom
//! event, a Room view).
//!
//! ## Where the row comes from
//!
//! A v1 action is handed the row it runs on, whatever started it. Here that
//! depends on the event: a table event binds `row`; a trigger a view's button
//! runs (`none`) is handed the row as its `payload`; and a workflow's step reads
//! the run's `context`. Every body generated here starts by naming the row from
//! the right one of the three, and then reads it as v1 did — with the row's
//! fields in scope by their own names, `row` and `table` beside them — so a v1
//! expression (`{ pages: Math.round(pages*0.9) }`) is pasted in unchanged.
//!
//! ## What the browser does
//!
//! Half of v1's actions only answer the browser: `navigate` answers `{ goto }`,
//! `toast` answers `{ notify }`. Saltcorn UI's page script is v1's own
//! `common_done`, so a body that answers the same object has the same effect.
//! Some of those answers are JavaScript for the page to run (`eval_js`: go back,
//! submit the form, copy to the clipboard), which the page can only do when the
//! application's content security policy allows `'unsafe-eval'`; such an action
//! says so ([`Converted::browser_eval`]) and the importer relaxes that one
//! policy, saying so in the report.

use serde_json::{Map, Value as Json, json};

use sc_error::{Error, Result};
use sc_expr::rewrite_free_refs;

use super::formulas::V1Schema;
use super::{name_of, text};

/// Every action v1's base plugin defines (`base-plugin/actions.ts`), each of
/// which [`convert_action`] answers for, and the two v1 trigger kinds that
/// name an action of their own.
pub(super) const V1_BUILTIN_ACTIONS: [&str; 33] = [
    "blocks",
    "emit_event",
    "loop_rows",
    "webhook",
    "find_or_create_dm_room",
    "send_email",
    "insert_joined_row",
    "duplicate_row",
    "recalculate_stored_fields",
    "insert_any_row",
    "modify_row",
    "delete_rows",
    "navigate",
    "step_control_flow",
    "form_action",
    "copy_to_clipboard",
    "toast",
    "run_js_code",
    "run_js_code_in_field",
    "duplicate_row_prefill_edit",
    "set_user_language",
    "sync_table_from_external",
    "reload_embedded_view",
    "progress_bar",
    "sleep",
    "refresh_user_session",
    "notify_user",
    "convert_session_to_user",
    "train_model_instance",
    "download_file_to_browser",
    "install_progressive_web_app",
    // Not in `actions.ts` itself but in v1's trigger model: a trigger whose
    // steps are each an action or another trigger.
    "Multi-step action",
    // v1's own name for the agent action, which `agents` converts; listed so a
    // view naming it is not mistaken for a trigger.
    "Agent",
];

/// Whether `name` is one of v1's built-in actions, rather than a trigger, a
/// view's own action or a plugin's.
pub(super) fn is_builtin(name: &str) -> bool {
    V1_BUILTIN_ACTIONS.contains(&name)
}

/// Where the row a converted action runs on is read from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RowSource {
    /// A table event's `row`.
    Event,
    /// The `payload` of a trigger something asked to run — a view's button, the
    /// Run button, another trigger.
    Payload,
    /// A workflow run's `context`.
    Context,
}

impl RowSource {
    fn binding(self) -> &'static str {
        match self {
            RowSource::Event => "row",
            RowSource::Payload => "payload",
            RowSource::Context => "context",
        }
    }
}

/// What a v1 action is converted in the light of.
pub(super) struct Origin<'a> {
    /// The table v1 ran it against: a trigger's table, a view's.
    pub table: Option<&'a str>,
    pub row: RowSource,
    /// A v1 expression over the row as one here. The identity for a trigger
    /// (the generated body puts the row's fields in scope by name, as v1 did);
    /// a workflow's own rewriting for a step, whose variables live elsewhere
    /// here ([`super::workflows`]).
    pub formula: &'a dyn Fn(&str) -> Result<String>,
    /// Names v1 had in scope that live elsewhere here, and where — a workflow's
    /// variables (`orders` → `context.step1`).
    pub bindings: Vec<(String, String)>,
}

/// What the whole backup says that an action may need: its schema, and its
/// triggers in pack order, which is the order v1 numbers them in when it
/// restores the pack (so `loop_rows`'s `trigger_id: 3` is the third).
pub(super) struct Site<'a> {
    pub schema: &'a V1Schema,
    pub triggers: &'a [String],
    /// The store v1's files are imported into, if the backup has any.
    pub store: Option<&'a str>,
}

/// A v1 action as one here.
#[derive(Debug)]
pub(super) struct Converted {
    pub action: String,
    pub configuration: Json,
    /// A condition the trigger itself must carry (v1's `send_email` "Only if").
    pub only_if: Option<String>,
    /// Whether an answer of this action is JavaScript for the page to run.
    pub browser_eval: bool,
    /// What the conversion could not carry, for the restore report.
    pub notes: Vec<String>,
}

impl Converted {
    fn code(code: String) -> Converted {
        Converted {
            action: "run_js_code".to_owned(),
            configuration: json!({ "code": code }),
            only_if: None,
            browser_eval: false,
            notes: Vec::new(),
        }
    }

    fn evaluating(mut self) -> Converted {
        self.browser_eval = true;
        self
    }

    fn noting(mut self, note: impl Into<String>) -> Converted {
        self.notes.push(note.into());
        self
    }
}

/// The v1 action `action`, configured with `config`, as an action here — or the
/// reason it cannot be.
pub(super) fn convert_action(
    action: &str,
    config: &Map<String, Json>,
    origin: &Origin<'_>,
    site: &Site<'_>,
) -> Result<Converted> {
    let js = Js { origin, site };
    let s = |key: &str| text(config, key);
    let flag = |key: &str| super::flag_value(config.get(key));
    Ok(match action {
        "run_js_code" | "blocks" => {
            let code = s("code");
            if s("run_where") == "Client page" {
                Converted::code(js.body(
                    action,
                    &format!(
                        "return {{ eval_js: {}, row, field_names: {} }};",
                        lit(&code),
                        js.field_list()
                    ),
                ))
                .evaluating()
            } else {
                Converted::code(js.v1_code(action, &code))
            }
        }
        "run_js_code_in_field" => run_js_code_in_field(&js, config)?,
        "emit_event" => {
            return Err(Error::invalid(
                "it emits an event, and this system raises its events itself: there is no \
                 way to emit one",
            ));
        }
        "loop_rows" => loop_rows(&js, config)?,
        "webhook" => webhook(&js, config)?,
        "find_or_create_dm_room" => {
            return Err(Error::invalid(
                "it opens a Room view, and this system has no Room view pattern",
            ));
        }
        "send_email" => send_email(origin, site, config)?,
        "insert_joined_row" => insert_joined_row(&js, config)?,
        "duplicate_row" => {
            let table = js.require_table(action)?;
            Converted::code(js.body(
                action,
                &format!(
                    "const __new = {{}};\n\
                     for (const __f of {}) if (__f !== table.pk_name && row[__f] !== undefined) \
                     __new[__f] = row[__f] !== null && typeof row[__f] === \"object\" && \
                     \"id\" in row[__f] ? row[__f].id : row[__f];\n\
                     await table.insertRow(__new, user);",
                    lit_list(&site.schema.stored_fields(table))
                ),
            ))
        }
        "recalculate_stored_fields" => Converted::code(js.body(
            action,
            "// A calculated field here is computed when it is read, so there is never a\n\
             // stored value to bring up to date.\n\
             return;",
        )),
        "insert_any_row" => insert_any_row(&js, config)?,
        "modify_row" => modify_row(&js, config)?,
        "delete_rows" => {
            if flag("delete_triggering_row") {
                js.require_table(action)?;
                Converted::code(js.body(
                    action,
                    "if (row[table.pk_name] === undefined || row[table.pk_name] === null)\n  \
                     throw new Error(\"delete_rows cannot find triggering row\");\n\
                     await table.deleteRows({ [table.pk_name]: row[table.pk_name] }, user);\n\
                     return {};",
                ))
            } else {
                Converted::code(js.body(
                    action,
                    &format!(
                        "await Table.findOne({}).deleteRows({}, user);\nreturn {{}};",
                        lit(&s("table_name")),
                        js.expr(&s("delete_where"))?
                    ),
                ))
            }
        }
        "navigate" => navigate(&js, config)?,
        "step_control_flow" => {
            let answer = match s("control_action").as_str() {
                "Halt steps" => "{ halt_steps: true }".to_owned(),
                "Goto step" => format!("{{ goto_step: {} }}", lit_value(config.get("step"))),
                "Clear return values" => "{ clear_return_values: true }".to_owned(),
                _ => "undefined".to_owned(),
            };
            Converted::code(js.body(action, &format!("return {answer};")))
        }
        "form_action" => form_action(&js, config)?,
        "copy_to_clipboard" => Converted::code(js.body(
            action,
            &format!(
                "return {{ eval_js: `navigator.clipboard.writeText(${{JSON.stringify({})}})` }};",
                js.interpolation(&s("text_template"))?
            ),
        ))
        .evaluating(),
        "toast" => toast(&js, config)?,
        "duplicate_row_prefill_edit" => {
            let prefill: Vec<String> = config
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix("_prefill_")
                        .filter(|_| super::flag_value(Some(v)))
                        .map(str::to_owned)
                })
                .collect();
            let prefill: Vec<&str> = prefill.iter().map(String::as_str).collect();
            Converted::code(js.body(
                action,
                &format!(
                    "const __qs = {}\n  .filter((k) => row[k] !== undefined && row[k] !== null)\n  \
                     .map((k) => `${{encodeURIComponent(k)}}=${{encodeURIComponent(row[k])}}`)\n  \
                     .join(\"&\");\n\
                     return {{ goto: `/view/{}?${{__qs}}` }};",
                    lit_list(&prefill),
                    template_text(&s("viewname"))
                ),
            ))
        }
        "set_user_language" => Converted::code(js.body(
            action,
            &format!(
                "if (user && user.id) await db.users.where({{ id: user.id }}).update({{ language: {} }});\n\
                 return {{ reload_page: true }};",
                lit(&s("language"))
            ),
        ))
        .noting(
            "it sets the language of a signed-in user only: for a visitor who is not signed in, \
             v1 set a cookie, which an action here cannot",
        ),
        "sync_table_from_external" => sync_table_from_external(&js, config)?,
        "reload_embedded_view" => reload_embedded_view(&js, config)?,
        "progress_bar" => {
            let interp = |key: &str| js.interpolation(&s(key));
            Converted::code(js.body(
                action,
                &format!(
                    "return {{ progress_bar_update: {{ blocking: {}, id: {}, close: {}, \
                     message: {}, title: {}, percent: +({}), maxHeight: {}, popupWidth: {} }} }};",
                    flag("blocking"),
                    interp("id")?,
                    flag("close"),
                    interp("message")?,
                    interp("title")?,
                    interp("percent")?,
                    lit_value(config.get("maxHeight")),
                    lit_value(config.get("popupWidth")),
                ),
            ))
        }
        "sleep" => {
            if s("sleep_where") == "Server" {
                return Err(Error::invalid(
                    "it waits on the server, and a code body here has no timers to wait with",
                ));
            }
            let ms = config.get("seconds").and_then(Json::as_f64).unwrap_or(0.0) * 1000.0;
            Converted::code(js.body(
                action,
                &format!(
                    "return {{ eval_js: \"return new Promise((resolve) => setTimeout(resolve, {ms}));\" }};"
                ),
            ))
            .evaluating()
        }
        "refresh_user_session" => Converted::code(js.body(
            action,
            "// A session here reads its user back from the users table rather than keeping a\n\
             // copy, so a change to the user's row is already what the next request sees.\n\
             return;",
        )),
        "notify_user" => {
            return Err(Error::invalid(
                "it sends an in-app notification, and this system has no notifications",
            ));
        }
        "convert_session_to_user" => {
            return Err(Error::invalid(
                "it reads the session a user had before signing in, and a sign-in here does \
                 not carry it",
            ));
        }
        "train_model_instance" => {
            return Err(Error::invalid(
                "it trains a v1 model, and v1's models are not imported",
            ));
        }
        "download_file_to_browser" => download_file_to_browser(&js, config)?,
        "install_progressive_web_app" => {
            return Err(Error::invalid(
                "it installs v1's progressive web app, which this system does not serve",
            ));
        }
        "Multi-step action" => {
            return Err(Error::invalid(
                "a multi-step action is converted with its steps, by the trigger that has them",
            ));
        }
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not one of Saltcorn 1's own actions; it may be a plugin's, and \
                 plugins are not imported"
            )));
        }
    })
}

/// v1's `Multi-step action` trigger as a body that runs its steps in order,
/// each as the trigger `step_trigger(i)` names, with v1's control flow: a
/// step's `only_if`, `goto_step`, `halt_steps`, `clear_return_values`, and a
/// step's `set_fields` seen by the steps after it.
pub(super) fn multi_step(
    origin: &Origin<'_>,
    site: &Site<'_>,
    steps: &[(String, String)],
) -> Result<Converted> {
    let js = Js { origin, site };
    let mut list = Vec::new();
    for (trigger, only_if) in steps {
        let condition = if only_if.trim().is_empty() {
            "null".to_owned()
        } else {
            format!("() => {}", js.expr(only_if)?)
        };
        list.push(format!("[{}, {condition}]", lit(trigger)));
    }
    Ok(Converted::code(js.body(
        "Multi-step action",
        &format!(
            "const __steps = [{}];\n\
             let __result = {{}};\n\
             for (let __i = 0, __n = 0; __i < __steps.length && __n < 200; __i++, __n++) {{\n  \
               const [__name, __when] = __steps[__i];\n  \
               if (__when && !__when()) continue;\n  \
               const __res = (await trigger(__name).run(row)) || {{}};\n  \
               if (__res.goto_step) {{ __i = +__res.goto_step - 2; delete __res.goto_step; }}\n  \
               if (__res.clear_return_values) __result = {{}};\n  \
               if (__res.set_fields) Object.assign(row, __res.set_fields);\n  \
               __merge(__result, __res);\n  \
               if (__result.error || __result.halt_steps) break;\n\
             }}\n\
             return __result;",
            list.join(", ")
        ),
    )))
}

// --- the actions with more to them ---------------------------------------------

fn run_js_code_in_field(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let field = text(config, "code_field");
    // v1 reads the code off the row, or one key away (`author.bio`).
    let read = match field.split_once('.') {
        None => format!("const __code = row[{}] || \"\";", lit(&field)),
        Some((key, target)) => {
            let table = js.require_table("run_js_code_in_field")?;
            let target_table = js
                .site
                .schema
                .key_target(table, key)
                .ok_or_else(|| Error::invalid(format!("`{key}` is not a key of `{table}`")))?;
            format!(
                "const __ref = row[{key}];\n\
                 const __code = (__ref !== null && typeof __ref === \"object\" ? __ref[{target}] \
                 : __ref === undefined || __ref === null ? \"\" : \
                 ((await Table.findOne({tt}).getRow({{ id: __ref }})) || {{}})[{target}]) || \"\";",
                key = lit(key),
                target = lit(target),
                tt = lit(target_table),
            )
        }
    };
    if text(config, "run_where") == "Client page" {
        return Ok(Converted::code(js.body(
            "run_js_code_in_field",
            &format!(
                "{read}\nreturn {{ eval_js: __code, row, field_names: {} }};",
                js.field_list()
            ),
        ))
        .evaluating());
    }
    Ok(Converted::code(js.body(
        "run_js_code_in_field",
        &format!(
            "{read}\n\
             const __run = new (Object.getPrototypeOf(async function () {{}}).constructor)(\n  \
             \"row\", \"table\", \"user\", \"__fields\", `with (__fields) {{ ${{__code}}\\n}}`);\n\
             return await __run(row, table, user, row);"
        ),
    )))
}

fn loop_rows(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let id = config
        .get("trigger_id")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()));
    let target = id
        .and_then(|id| usize::try_from(id).ok()?.checked_sub(1))
        .and_then(|i| js.site.triggers.get(i))
        .ok_or_else(|| {
            Error::invalid(format!(
                "it runs the trigger numbered {} for each row, which is not one of the backup's",
                id.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
            ))
        })?;
    let mut options = Vec::new();
    if let Some(order) = config
        .get("orderBy")
        .and_then(Json::as_str)
        .filter(|o| !o.is_empty())
    {
        options.push(format!("orderBy: {}", lit(order)));
    }
    if super::flag_value(config.get("orderDesc")) {
        options.push("orderDesc: true".to_owned());
    }
    if let Some(limit) = config.get("limit").filter(|l| !l.is_null() && *l != "") {
        options.push(format!("limit: {}", lit_value(Some(limit))));
    }
    let where_ = match text(config, "where").trim() {
        "" => "{}".to_owned(),
        w => js.expr(w)?,
    };
    let mut converted = Converted::code(js.body(
        "loop_rows",
        &format!(
            "const __rows = await Table.findOne({}).getRows({where_}, {{ {} }});\n\
             const __result = {{}};\n\
             for (const __row of __rows) __merge(__result, await trigger({}).run(__row));\n\
             return __result;",
            lit(&text(config, "table_name")),
            options.join(", "),
            lit(target),
        ),
    ));
    if config.get("interval").and_then(Json::as_f64).unwrap_or(0.0) > 0.0 {
        converted = converted
            .noting("it no longer waits between rows: a code body here has no timers to wait with");
    }
    Ok(converted)
}

fn webhook(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let s = |key: &str| text(config, key);
    let method = match s("method").trim() {
        "" => "POST".to_owned(),
        m => m.to_uppercase(),
    };
    let body = match s("body").trim() {
        "" => "JSON.stringify(row)".to_owned(),
        b => format!("JSON.stringify(await (async () => {})())", js.expr(b)?),
    };
    let mut code = format!(
        "const __opts = {{ method: {}, headers: {{ \"Content-Type\": \"application/json\" }} }};\n\
         if (__opts.method !== \"GET\") __opts.body = {body};\n",
        lit(&method)
    );
    if !s("authorization").trim().is_empty() {
        code.push_str(&format!(
            "__opts.headers.Authorization = {};\n",
            js.interpolation(&s("authorization"))?
        ));
    }
    code.push_str(&format!(
        "const __res = await fetch({}, __opts);\n\
         const __json = (__res.headers.get(\"content-type\") || \"\").includes(\"application/json\");\n",
        js.interpolation(&s("url"))?
    ));
    let var = s("response_var");
    let field = s("response_field");
    if !var.is_empty() {
        code.push_str(&format!(
            "return {{ {}: __json ? await __res.json() : await __res.text() }};",
            lit(&var)
        ));
    } else if !field.is_empty() {
        let table = js.require_table("webhook")?;
        // v1 keeps a JSON answer as its text in a String field.
        let as_text = matches!(
            js.site.schema.type_and_label(table, &field),
            Some(("String", _))
        );
        code.push_str(&format!(
            "const __body = __json ? await __res.json() : await __res.text();\n\
             await table.updateRow({{ {}: {} }}, row[table.pk_name]);\n\
             return;",
            lit(&field),
            if as_text {
                "__json ? JSON.stringify(__body) : __body"
            } else {
                "__body"
            }
        ));
    } else {
        code.push_str("return;");
    }
    Ok(Converted::code(js.body("webhook", &code)))
}

fn insert_joined_row(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let relation = text(config, "joined_table");
    let (table, key) = relation
        .split_once('.')
        .ok_or_else(|| Error::invalid(format!("its relation `{relation}` is not `table.key`")))?;
    // v1 fills every key to users in the new row with the user who acted.
    let to_users: Vec<String> = js
        .site
        .schema
        .field_names(table)
        .into_iter()
        .filter(|f| *f != key && js.site.schema.key_target(table, f) == Some("users"))
        .map(|f| format!("  if (user && user.id) __new[{}] = user.id;\n", lit(f)))
        .collect();
    Ok(Converted::code(js.body(
        "insert_joined_row",
        &format!(
            "const __new = {{ [{}]: row.id }};\n{}return await Table.findOne({}).insertRow(__new, user);",
            lit(key),
            to_users.concat(),
            lit(table)
        ),
    )))
}

fn insert_any_row(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let id_var = text(config, "id_variable");
    Ok(Converted::code(js.body(
        "insert_any_row",
        &format!(
            "const __calc = await (async () => {})();\n\
             const __t = Table.findOne({});\n\
             const __ids = [];\n\
             const __one = async (__r) => {{\n  \
               const __pk = __r[__t.pk_name];\n  \
               if (__pk !== undefined && __pk !== null && (await __t.getRow({{ [__t.pk_name]: __pk }}))) {{\n    \
                 await __t.updateRow(__r, __pk, user);\n    \
                 __ids.push(__pk);\n  \
               }} else __ids.push(await __t.insertRow(__r, user));\n\
             }};\n\
             if (Array.isArray(__calc)) for (const __r of __calc) await __one(__r);\n\
             else await __one(__calc);\n\
             return {};",
            js.expr(&text(config, "row_expr"))?,
            lit(&text(config, "table")),
            if id_var.is_empty() {
                "{}".to_owned()
            } else {
                format!(
                    "{{ {}: Array.isArray(__calc) ? __ids : __ids[0] }}",
                    lit(&id_var)
                )
            }
        ),
    )))
}

fn modify_row(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let calc = js.expr(&text(config, "row_expr"))?;
    let code = match text(config, "where").as_str() {
        // The values go back to the form on the page, which v1's own script
        // fills in.
        "Form" | "Filter state" | "Row" | "Active edit view" => {
            format!("return {{ set_fields: {calc} }};")
        }
        _ if !text(config, "select_table").is_empty() && !text(config, "query").is_empty() => {
            format!(
                "const __calc = {calc};\n\
                 const __t = Table.findOne({});\n\
                 for (const __r of await __t.getRows({})) await __t.updateRow(__calc, __r[__t.pk_name]);\n\
                 return;",
                lit(&text(config, "select_table")),
                js.expr(&text(config, "query"))?
            )
        }
        _ => {
            js.require_table("modify_row")?;
            format!(
                "const __res = await table.tryUpdateRow({calc}, row[table.pk_name], user);\n\
                 if (__res && __res.error) return __res;\n\
                 return;"
            )
        }
    };
    Ok(Converted::code(js.body("modify_row", &code)))
}

fn navigate(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let s = |key: &str| text(config, key);
    let key = if super::flag_value(config.get("in_popup")) {
        "popup"
    } else {
        "goto"
    };
    let target = if super::flag_value(config.get("new_tab")) {
        ", target: \"_blank\""
    } else {
        ""
    };
    let state = match s("state_formula").trim() {
        "" => "\"\"".to_owned(),
        formula => format!(
            "\"?\" + Object.entries({}).map(([k, v]) => \
             `${{encodeURIComponent(k)}}=${{encodeURIComponent(v)}}`).join(\"&\")",
            js.expr(formula)?
        ),
    };
    let answer = |answer: String| Converted::code(js.body("navigate", &answer));
    Ok(match s("nav_action").as_str() {
        "Go to URL" => answer(format!(
            "return {{ {key}: {}{target} }};",
            js.interpolation(&s("url"))?
        )),
        "Go to Page" => answer(format!(
            "return {{ {key}: `/page/{}` + {state}{target} }};",
            template_text(&s("page"))
        )),
        "Go to View" => answer(format!(
            "return {{ {key}: `/view/{}` + {state}{target} }};",
            template_text(&s("view"))
        )),
        "Popup modal" => answer(format!(
            "return {{ popup: {} }};",
            js.interpolation(&s("url"))?
        )),
        "Reload page" => answer("return { reload_page: true };".to_owned()),
        "Back" => answer("return { eval_js: \"history.back()\" };".to_owned()).evaluating(),
        "Close tab" => answer("return { eval_js: \"window.close()\" };".to_owned()).evaluating(),
        "Close modal" => {
            answer("return { eval_js: \"close_saltcorn_modal()\" };".to_owned()).evaluating()
        }
        other => {
            return Err(Error::invalid(format!(
                "its navigation `{other}` is not one v1 has"
            )));
        }
    })
}

fn form_action(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    const FORM: &str = "$('form[data-viewname=\"'+viewname+'\"]')";
    let eval = |script: String| {
        Converted::code(js.body(
            "form_action",
            &format!("return {{ eval_js: {} }};", lit(&script)),
        ))
        .evaluating()
    };
    Ok(match text(config, "form_action").as_str() {
        "Submit" => eval(format!("{FORM}.submit()")),
        "Reset" => eval(format!("{FORM}.trigger('reset')")),
        "Submit with Ajax" => eval(format!("submitWithAjax({FORM})")),
        "Ajax Save Form Data" => eval(format!("ajaxSubmitForm({FORM}, true)")),
        "Save" => {
            js.require_table("form_action")?;
            Converted::code(js.body(
                "form_action",
                &format!(
                    "if (row[table.pk_name] === undefined || row[table.pk_name] === null || row[table.pk_name] === \"\") {{\n  \
                       const __res = await table.tryInsertRow(row, user);\n  \
                       if (__res.success) return {{ notify_success: \"Saved\", set_fields: {{ [table.pk_name]: __res.success }} }};\n  \
                       return {{ eval_js: {} }};\n\
                     }}\n\
                     const __res = await table.tryUpdateRow(row, row[table.pk_name], user);\n\
                     return __res && __res.error ? __res : {{ notify_success: \"Saved\" }};",
                    lit(&format!("return saveAndContinueAsync({FORM})"))
                ),
            ))
            .evaluating()
            .noting("its Save does not store files uploaded in the form")
        }
        _ => eval(format!("{FORM}[0].requestSubmit()")),
    })
}

fn toast(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let s = |key: &str| text(config, key);
    let mut extra = String::new();
    if !s("title").is_empty() {
        extra.push_str(&format!(
            ", toast_title: {}",
            js.interpolation(&s("title"))?
        ));
    }
    if let Some(delay) = config
        .get("remove_delay")
        .filter(|d| !d.is_null() && **d != "")
    {
        extra.push_str(&format!(", remove_delay: {}", lit_value(Some(delay))));
    }
    let message = js.interpolation(&s("text"))?;
    let kind = match s("notify_type").as_str() {
        "" => s("type"),
        kind => kind.to_owned(),
    };
    let answer = match kind.as_str() {
        "Error" => format!("{{ error: {message}{extra} }}"),
        "Success" => format!("{{ notify_success: {message}{extra} }}"),
        "Warning" => format!("{{ notify: {message}, notify_type: \"warning\"{extra} }}"),
        _ => format!("{{ notify: {message}{extra} }}"),
    };
    Ok(Converted::code(
        js.body("toast", &format!("return {answer};")),
    ))
}

fn sync_table_from_external(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let s = |key: &str| text(config, key);
    let (source, dest, pk) = (s("table_src"), s("table_dest"), s("pk_field"));
    let schema = js.site.schema;
    // v1 matches a destination field to the source field with the same name or
    // label, either way round.
    let mapping = if config
        .get("match_field_names")
        .is_none_or(|v| super::flag_value(Some(v)))
    {
        let pairs: Vec<String> = schema
            .field_names(&dest)
            .into_iter()
            .filter_map(|d| {
                let (_, d_label) = schema.type_and_label(&dest, d)?;
                let s = schema.field_names(&source).into_iter().find(|s| {
                    let s_label = schema.type_and_label(&source, s).map_or("", |(_, l)| l);
                    *s == d || s_label == d_label || *s == d_label || s_label == d
                })?;
                Some(format!("{}: __src[{}]", lit(d), lit(s)))
            })
            .collect();
        format!("({{ {} }})", pairs.join(", "))
    } else {
        // The row expression is over the *source* row's fields.
        let fields = identifiers(schema.field_names(&source));
        format!(
            "(({{ {} }}) => ({}))(__src)",
            fields.join(", "),
            js.expr(&s("row_expr"))?
        )
    };
    let where_ = match s("where").trim() {
        "" => "{}".to_owned(),
        w => js.expr(w)?,
    };
    Ok(Converted::code(js.body(
        "sync_table_from_external",
        &format!(
            "const __source = Table.findOne({src});\n\
             if (!__source) return {{ error: \"Source table not found\" }};\n\
             const __dest = Table.findOne({dst});\n\
             const __srcPk = __source.pk_name;\n\
             const __map = (__src) => ({{ [{pk}]: __src[__srcPk], ...{mapping} }});\n\
             const __srcRows = await __source.getRows({where_});\n\
             const __destRows = await __dest.getRows({{}});\n\
             const __srcPks = new Set(__srcRows.map((r) => r[__srcPk]));\n\
             const __destByPk = new Map(__destRows.map((r) => [r[{pk}], r]));\n\
             for (const __src of __srcRows) {{\n  \
               const __new = __map(__src);\n  \
               const __old = __destByPk.get(__src[__srcPk]);\n  \
               if (!__old) {{ await __dest.insertRow(__new, user); continue; }}\n  \
               const __upd = {{}};\n  \
               for (const k of Object.keys(__new)) if (__new[k] != __old[k]) __upd[k] = __new[k];\n  \
               if (Object.keys(__upd).length) await __dest.updateRow(__upd, __old[__dest.pk_name], user);\n\
             }}\n{}",
            if super::flag_value(config.get("delete_rows")) || !config.contains_key("delete_rows") {
                format!(
                    "const __gone = [...__destByPk.keys()].filter((k) => !__srcPks.has(k));\n\
                     if (__gone.length) await __dest.deleteRows({{ [{pk}]: {{ in: __gone }} }}, user);\n"
                )
            } else {
                String::new()
            },
            src = lit(&source),
            dst = lit(&dest),
            pk = lit(&pk),
        ),
    )))
}

fn reload_embedded_view(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let view = text(config, "view");
    let state = match text(config, "new_state_fml").trim() {
        "" => None,
        formula => Some(format!(
            "Object.entries({}).map(([k, v]) => \
             `${{encodeURIComponent(k)}}=${{encodeURIComponent(v)}}`).join(\"&\")",
            js.expr(formula)?
        )),
    };
    let interval = config
        .get("interval")
        .and_then(Json::as_i64)
        .filter(|i| *i > 0);
    Ok(match interval {
        // The page's script reloads a view by name; only a timer needs a script.
        None => Converted::code(js.body(
            "reload_embedded_view",
            &format!(
                "return {{ reload_embedded_view: {}{} }};",
                lit(&view),
                state.map(|s| format!(", new_state: {s}")).unwrap_or_default()
            ),
        )),
        Some(ms) => Converted::code(js.body(
            "reload_embedded_view",
            &format!(
                "const __args = [{}{}].map((a) => JSON.stringify(a)).join(\", \");\n\
                 return {{ eval_js: `setInterval(() => {{ reload_embedded_view(${{__args}}) }}, {ms})` }};",
                lit(&view),
                state.map(|s| format!(", {s}")).unwrap_or_default()
            ),
        ))
        .evaluating(),
    })
}

fn download_file_to_browser(js: &Js<'_>, config: &Map<String, Json>) -> Result<Converted> {
    let path = match config.get("filepath_expr").and_then(Json::as_str) {
        Some(expr) if !expr.trim().is_empty() => js.expr(expr)?,
        _ => format!("row[{}]", lit(&text(config, "file_field"))),
    };
    // v1's one file area is the store the import puts its files in.
    let store = js.site.store.ok_or_else(|| {
        Error::invalid("it downloads one of v1's files, and the backup has no files")
    })?;
    Ok(Converted::code(js.body(
        "download_file_to_browser",
        &format!(
            "const __path = {path};\n\
             if (!__path) return;\n\
             const __file = fs({}).open(__path);\n\
             const __bytes = await __file.bytes();\n\
             let __bin = \"\";\n\
             for (let i = 0; i < __bytes.length; i += 0x8000) \
             __bin += String.fromCharCode(...__bytes.subarray(i, i + 0x8000));\n\
             return {{ download: {{ filename: __path.split(\"/\").pop(), \
             mimetype: (await __file.stat()).mimeType || \"application/octet-stream\", blob: btoa(__bin) }} }};",
            lit(store)
        ),
    )))
}

/// v1's `send_email` as this system's: the recipients, subject and body as
/// templates. v1's other body, a view rendered into the message, has no
/// counterpart and is refused.
fn send_email(
    origin: &Origin<'_>,
    site: &Site<'_>,
    config: &Map<String, Json>,
) -> Result<Converted> {
    let s = |key: &str| text(config, key);
    let tpl = |source: &str| template(origin, site, source);
    let mut out = Map::new();
    let mut notes = Vec::new();
    let mut only_if = None;
    for (from, to) in [("cc_email", "cc"), ("bcc_email", "bcc")] {
        if !s(from).trim().is_empty() {
            out.insert(to.to_owned(), json!(tpl(&s(from))?));
        }
    }
    // A workflow's step is v1's other form of the action: everything a template.
    if origin.row == RowSource::Context || !config.contains_key("body_type") {
        out.insert("to".to_owned(), json!(tpl(&s("to_email"))?));
        out.insert("subject".to_owned(), json!(tpl(&s("subject"))?));
        out.insert("html".to_owned(), json!(tpl(&s("body"))?));
        if !s("confirm_field").is_empty() {
            notes.push("it does not set its send confirmation variable".to_owned());
        }
    } else {
        let field = s("body_field");
        match s("body_type").as_str() {
            "Text field" => {
                out.insert("text".to_owned(), json!(field_token(origin, &field, false)));
            }
            "HTML field" => {
                out.insert("html".to_owned(), json!(field_token(origin, &field, true)));
            }
            "MJML field" => {
                out.insert("html".to_owned(), json!(field_token(origin, &field, true)));
                out.insert("mjml".to_owned(), json!(true));
            }
            _ => {
                return Err(Error::invalid(format!(
                    "its message is the view `{}` rendered for the row, and this system cannot \
                     render a view into an email; give it an HTML field or a template instead",
                    s("viewname")
                )));
            }
        }
        let to = match s("to_email").as_str() {
            "User" => "{{ user.email }}".to_owned(),
            "Field" => {
                let field = s("to_email_field");
                let to_users =
                    origin.table.and_then(|t| site.schema.key_target(t, &field)) == Some("users");
                match (to_users, origin.row) {
                    (false, _) => field_token(origin, &field, false),
                    (true, RowSource::Event) => format!("{{{{ {field}{}email }}}}", sc_expr::JOIN),
                    (true, _) => {
                        return Err(Error::invalid(format!(
                            "it sends to the user its row's `{field}` points at, which a \
                             template here can only follow from a table event's row"
                        )));
                    }
                }
            }
            _ => tpl(&s("to_email_fixed"))?,
        };
        out.insert("to".to_owned(), json!(to));
        let subject = if super::flag_value(config.get("subject_formula")) {
            format!("{{{{ {} }}}}", template_expr(origin, site, &s("subject"))?)
        } else {
            s("subject")
        };
        out.insert("subject".to_owned(), json!(subject));
        let path = s("attachment_path");
        if !path.is_empty() {
            if path.contains('.') {
                notes.push(format!(
                    "it does not attach the files of the related rows `{path}`; only a File \
                     field of the row itself can be attached here"
                ));
            } else {
                out.insert(format!("attach_{path}"), json!(true));
            }
        }
        let condition = s("only_if");
        if !condition.trim().is_empty() {
            if origin.row != RowSource::Event {
                return Err(Error::invalid(
                    "it has an \"Only if\", which a trigger here can only test on a table event",
                ));
            }
            only_if = Some(condition);
        }
        if !s("confirm_field").is_empty() {
            notes.push(format!(
                "it does not set the confirmation field `{}` after sending",
                s("confirm_field")
            ));
        }
    }
    Ok(Converted {
        action: "send_email".to_owned(),
        configuration: Json::Object(out),
        only_if,
        browser_eval: false,
        notes,
    })
}

// --- generating JavaScript ---------------------------------------------------------

/// The generator of one action's body, in the light of where it runs.
struct Js<'a> {
    origin: &'a Origin<'a>,
    site: &'a Site<'a>,
}

impl Js<'_> {
    /// `inner` with v1's scope around it: `row` (the row, wherever it came
    /// from), `table` (its table, in v1's `Table`), the row's fields by name, and
    /// a workflow's variables by theirs.
    ///
    /// The scope is the parameters of a function, rather than declarations, so
    /// that nothing in `inner` can collide with it: a v1 field that happens to
    /// be called what a body here binds (`payload`, `user`) is simply not put in
    /// scope by name. `__merge` is v1's `mergeActionResults`, for the bodies
    /// that run several actions and answer one result.
    fn body(&self, action: &str, inner: &str) -> String {
        let source = self.origin.row.binding();
        let table = match self.origin.table {
            Some(table) => format!("Table.findOne({})", lit(table)),
            None => "undefined".to_owned(),
        };
        let (names, values) = self.scope();
        let merge = if inner.contains("__merge(") {
            "const __merge = (into, res) => {\n  \
               for (const [k, v] of Object.entries(res || {})) {\n    \
                 if (k === \"set_fields\") into.set_fields = { ...(into.set_fields || {}), ...v };\n    \
                 else if (![\"notify\", \"notify_success\", \"error\", \"eval_js\", \"download\"].includes(k) \
             || into[k] === undefined) into[k] = v;\n    \
                 else into[k] = [].concat(into[k], [v]);\n  \
               }\n\
             };\n"
        } else {
            ""
        };
        format!(
            "// Saltcorn 1's `{action}`, converted when its backup was imported.\n\
             return await (async (row, table, {{ {names} }}) => {{\n\
             {merge}{inner}\n\
             }})({source} ?? {{}}, {table}, {values});\n"
        )
    }

    /// v1's own `run_js_code` body, in v1's scope: `context` is the row too,
    /// `channel` its table's name, and `Actions.x(…)` runs the trigger `x` —
    /// all as v1 had them. The body is a function of its own inside that scope,
    /// so it may declare any name it likes, a field's included.
    fn v1_code(&self, action: &str, code: &str) -> String {
        self.body(
            action,
            &format!(
                "const __v1 = async (context, channel, Actions, fetchJSON, sleep, emitEvent) => {{\n\
                 {code}\n\
                 }};\n\
                 return await __v1(\n  \
                   row,\n  \
                   {},\n  \
                   new Proxy({{}}, {{ get: (_, name) => (args = {{}}) => trigger(String(name)).run({{ ...row, ...args }}) }}),\n  \
                   async (...args) => (await fetch(...args)).json(),\n  \
                   async () => {{ throw new Error(\"sleep() is not available: a code body here has no timers\"); }},\n  \
                   () => {{ throw new Error(\"emitEvent() is not available: this system raises its events itself\"); }},\n\
                 );",
                self.origin.table.map_or_else(|| "undefined".to_owned(), lit)
            ),
        )
    }

    /// The destructuring parameter that puts names in scope, and the value it
    /// destructures.
    fn scope(&self) -> (String, String) {
        if !self.origin.bindings.is_empty() {
            let names: Vec<&str> = self
                .origin
                .bindings
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| is_identifier(name))
                .collect();
            let values: Vec<String> = self
                .origin
                .bindings
                .iter()
                .filter(|(name, _)| is_identifier(name))
                .map(|(name, path)| format!("{name}: {path}"))
                .collect();
            return (names.join(", "), format!("{{ {} }}", values.join(", ")));
        }
        let names = self
            .origin
            .table
            .map(|t| identifiers(self.site.schema.field_names(t)))
            .unwrap_or_default();
        (
            names.join(", "),
            format!("{} ?? {{}}", self.origin.row.binding()),
        )
    }

    /// A v1 expression, as JavaScript in this body's scope.
    fn expr(&self, source: &str) -> Result<String> {
        if source.trim().is_empty() {
            return Ok("undefined".to_owned());
        }
        Ok(format!("({})", (self.origin.formula)(source)?))
    }

    /// A v1 `{{ }}` interpolation as a JavaScript template literal.
    fn interpolation(&self, source: &str) -> Result<String> {
        let mut out = String::from("`");
        for part in interpolation_parts(source) {
            match part {
                Part::Text(text) => out.push_str(&template_text(text)),
                Part::Expr(expr) => {
                    out.push_str(&format!("${{{} ?? \"\"}}", self.expr(expr)?));
                }
            }
        }
        out.push('`');
        Ok(out)
    }

    /// The table's field names as a JavaScript array, or `undefined`.
    fn field_list(&self) -> String {
        match self.origin.table {
            Some(t) => lit_list(&self.site.schema.field_names(t)),
            None => "undefined".to_owned(),
        }
    }

    /// The origin's table, for an action that only means something with one.
    fn require_table(&self, action: &str) -> Result<&str> {
        self.origin.table.ok_or_else(|| {
            Error::invalid(format!(
                "`{action}` acts on its row's table, and it has no table here"
            ))
        })
    }
}

/// A v1 interpolation as this system's template: each `{{ }}` expression read
/// where the origin keeps the row.
fn template(origin: &Origin<'_>, site: &Site<'_>, source: &str) -> Result<String> {
    let mut out = String::new();
    for part in interpolation_parts(source) {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Expr(expr) => {
                out.push_str(&format!("{{{{ {} }}}}", template_expr(origin, site, expr)?));
            }
        }
    }
    Ok(out)
}

/// A v1 expression over the row as a template's: unchanged for a table event,
/// whose row's fields a template names bare; read off `payload` for a trigger
/// something asked to run; and the workflow's own rewriting for a step.
fn template_expr(origin: &Origin<'_>, site: &Site<'_>, source: &str) -> Result<String> {
    match origin.row {
        RowSource::Event => Ok(source.trim().to_owned()),
        RowSource::Context => (origin.formula)(source),
        RowSource::Payload => {
            let fields = origin
                .table
                .map(|t| site.schema.field_names(t))
                .unwrap_or_default();
            rewrite_free_refs(source, &mut |path| {
                let head = path[0].as_str();
                if head == "row" {
                    Some((1, "payload".to_owned()))
                } else if fields.contains(&head) {
                    Some((1, format!("payload.{head}")))
                } else {
                    None
                }
            })
        }
    }
}

/// The token that puts a field of the row into a message: escaped as text,
/// or as it is where it is the HTML body.
fn field_token(origin: &Origin<'_>, field: &str, raw: bool) -> String {
    let path = match origin.row {
        RowSource::Event => field.to_owned(),
        _ => format!("{}.{field}", origin.row.binding()),
    };
    if raw {
        format!("{{{{! {path} }}}}")
    } else {
        format!("{{{{ {path} }}}}")
    }
}

enum Part<'a> {
    Text(&'a str),
    Expr(&'a str),
}

/// v1's `interpolate`: text, and the expressions between `{{` and `}}`.
fn interpolation_parts(source: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        if start > 0 {
            parts.push(Part::Text(&rest[..start]));
        }
        parts.push(Part::Expr(rest[start + 2..start + 2 + len].trim()));
        rest = &rest[start + 2 + len + 2..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    parts
}

/// Text as it may stand inside a JavaScript template literal.
fn template_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${")
}

/// A string as a JavaScript literal.
fn lit(text: &str) -> String {
    Json::from(text).to_string()
}

/// A JSON setting as a JavaScript literal; `undefined` when absent.
fn lit_value(value: Option<&Json>) -> String {
    match value {
        None | Some(Json::Null) => "undefined".to_owned(),
        Some(value) => value.to_string(),
    }
}

fn lit_list(items: &[&str]) -> String {
    Json::from(items.to_vec()).to_string()
}

/// The names that can be put in scope as variables: JavaScript identifiers,
/// less the reserved words and the names a body here binds itself.
fn identifiers<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    names.into_iter().filter(|n| is_identifier(n)).collect()
}

fn is_identifier(name: &str) -> bool {
    const TAKEN: &[&str] = &[
        // What the generated scope or this system's code bodies bind.
        "row",
        "table",
        "user",
        "old",
        "payload",
        "context",
        "session",
        "app",
        "db",
        "fetch",
        "fs",
        "trigger",
        "modfn",
        "models",
        "Table",
        "Field",
        "console",
        "undefined",
        "NaN",
        "Infinity",
        "arguments",
        "eval",
        // JavaScript's reserved words.
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "new",
        "null",
        "return",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
        "let",
        "static",
        "implements",
        "interface",
        "package",
        "private",
        "protected",
        "public",
        "await",
    ];
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !name.starts_with("__")
        && !TAKEN.contains(&name)
}

/// v1's configuration objects are sometimes absent, sometimes `null`.
pub(super) fn configuration_of(value: Option<&Json>) -> Map<String, Json> {
    value.and_then(Json::as_object).cloned().unwrap_or_default()
}

/// The `(trigger name, only_if)` a v1 multi-step trigger's step runs, and —
/// when the step is a built-in action rather than a trigger — the
/// configuration the trigger made of it is converted from.
pub(super) fn multi_step_steps(trigger: &Map<String, Json>) -> Vec<(String, Map<String, Json>)> {
    configuration_of(trigger.get("configuration"))
        .get("steps")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_object)
        .map(|step| (text(step, "step_action_name"), step.clone()))
        .collect()
}

/// The name of a trigger made of one of v1's actions where v1 had none: a
/// view's or page's button, a multi-step trigger's step.
pub(super) fn made_trigger_name(owner: &str, action: &str, n: usize) -> String {
    format!("{owner} · {action} {n}")
}

/// Whether a v1 trigger is a multi-step one.
pub(super) fn is_multi_step(trigger: &Map<String, Json>) -> bool {
    text(trigger, "action") == "Multi-step action"
}

/// The pack's trigger names, in pack order.
pub(super) fn pack_trigger_names(triggers: &[Json]) -> Vec<String> {
    triggers
        .iter()
        .filter_map(Json::as_object)
        .map(name_of)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> V1Schema {
        V1Schema::of(
            json!({ "tables": [
                { "name": "Books", "fields": [
                    { "name": "id", "label": "ID", "type": "Integer" },
                    { "name": "title", "label": "Title", "type": "String" },
                    { "name": "pages", "label": "Pages", "type": "Integer" },
                    { "name": "owner", "label": "Owner", "type": "Key",
                      "attributes": { "reftable_name": "users" } },
                ] },
            ] })
            .as_object()
            .unwrap(),
        )
    }

    fn identity(source: &str) -> Result<String> {
        Ok(source.to_owned())
    }

    fn convert(action: &str, config: Json, row: RowSource) -> Result<Converted> {
        let schema = schema();
        let triggers = vec!["Other".to_owned(), "PerRow".to_owned()];
        let origin = Origin {
            table: Some("Books"),
            row,
            formula: &identity,
            bindings: Vec::new(),
        };
        convert_action(
            action,
            config.as_object().unwrap(),
            &origin,
            &Site {
                schema: &schema,
                triggers: &triggers,
                store: Some("BooksDB"),
            },
        )
    }

    fn code(converted: &Converted) -> &str {
        converted.configuration["code"].as_str().unwrap()
    }

    /// Every action v1 has gets an answer: converted, or refused with a reason
    /// — never passed through under its v1 name to mean something else here.
    #[test]
    fn every_builtin_action_is_converted_or_refused_with_a_reason() {
        for action in V1_BUILTIN_ACTIONS {
            if matches!(action, "Agent" | "Multi-step action") {
                continue;
            }
            match convert(action, json!({}), RowSource::Payload) {
                Ok(converted) => assert!(
                    matches!(converted.action.as_str(), "run_js_code" | "send_email"),
                    "{action} became {}",
                    converted.action
                ),
                Err(e) => assert!(!e.to_string().is_empty(), "{action}"),
            }
        }
    }

    #[test]
    fn a_rows_fields_are_in_scope_by_name_wherever_the_row_came_from() {
        let converted = convert(
            "modify_row",
            json!({ "row_expr": "{ pages: Math.round(pages*0.9)}" }),
            RowSource::Payload,
        )
        .unwrap();
        let code = code(&converted);
        assert!(
            code.contains("async (row, table, { id, title, pages, owner }) =>")
                || code.contains("async (row, table, { id, owner, pages, title }) =>"),
            "{code}"
        );
        assert!(
            code.contains("})(payload ?? {}, Table.findOne(\"Books\"), payload ?? {});"),
            "{code}"
        );
        assert!(
            code.contains(
                "table.tryUpdateRow(({ pages: Math.round(pages*0.9)}), row[table.pk_name], user)"
            ),
            "{code}"
        );
    }

    #[test]
    fn what_answers_the_browser_answers_v1s_object() {
        let toast = convert(
            "toast",
            json!({ "notify_type": "Success", "text": "Saved {{title}} `ok`" }),
            RowSource::Payload,
        )
        .unwrap();
        assert!(
            code(&toast)
                .contains("return { notify_success: `Saved ${(title) ?? \"\"} \\`ok\\`` };"),
            "{}",
            code(&toast)
        );
        assert!(!toast.browser_eval);
        let back = convert(
            "navigate",
            json!({ "nav_action": "Back" }),
            RowSource::Payload,
        )
        .unwrap();
        assert!(back.browser_eval);
        let view = convert(
            "navigate",
            json!({ "nav_action": "Go to View", "view": "Show Books", "state_formula": "{id}" }),
            RowSource::Payload,
        )
        .unwrap();
        assert!(
            code(&view)
                .contains("return { goto: `/view/Show Books` + \"?\" + Object.entries(({id}))"),
            "{}",
            code(&view)
        );
    }

    #[test]
    fn a_v1_email_becomes_templates_and_a_rendered_view_is_refused() {
        let email = convert(
            "send_email",
            json!({ "body_type": "HTML field", "body_field": "title", "to_email": "Field",
                    "to_email_field": "owner", "subject": "About {{title}}", "only_if": "pages > 2" }),
            RowSource::Event,
        )
        .unwrap();
        assert_eq!(email.action, "send_email");
        assert_eq!(
            email.configuration,
            json!({ "html": "{{! title }}", "to": "{{ ownerⱵemail }}", "subject": "About {{title}}" })
        );
        assert_eq!(email.only_if.as_deref(), Some("pages > 2"));
        let fixed = convert(
            "send_email",
            json!({ "body_type": "Text field", "body_field": "title", "to_email": "Fixed",
                    "to_email_fixed": "{{title}}@example.com", "subject": "x" }),
            RowSource::Payload,
        )
        .unwrap();
        assert_eq!(
            fixed.configuration["to"],
            json!("{{ payload.title }}@example.com")
        );
        assert_eq!(fixed.configuration["text"], json!("{{ payload.title }}"));
        let e = convert(
            "send_email",
            json!({ "body_type": "View", "viewname": "Show Books", "to_email": "User" }),
            RowSource::Event,
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot render a view"), "{e}");
    }

    #[test]
    fn loop_rows_names_its_trigger_by_its_place_in_the_pack() {
        let converted = convert(
            "loop_rows",
            json!({ "table_name": "Books", "where": "{pages: {gt: 3}}", "trigger_id": 2,
                    "orderBy": "title", "limit": 5, "interval": 1 }),
            RowSource::Payload,
        )
        .unwrap();
        let code = code(&converted);
        assert!(
            code.contains("getRows(({pages: {gt: 3}}), { orderBy: \"title\", limit: 5 })"),
            "{code}"
        );
        assert!(code.contains("trigger(\"PerRow\").run(__row)"), "{code}");
        assert_eq!(converted.notes.len(), 1);
        assert!(convert("loop_rows", json!({ "trigger_id": 9 }), RowSource::Payload).is_err());
    }

    #[test]
    fn a_field_called_what_a_body_binds_is_not_put_in_scope() {
        assert!(is_identifier("pages"));
        for name in ["payload", "user", "class", "my field", "__x", "1st"] {
            assert!(!is_identifier(name), "{name}");
        }
    }
}
