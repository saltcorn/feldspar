//! Changing which triggers an application exposes: `update_application`'s
//! `triggers` section and `feldspar app triggers`, one rule for both.
//!
//! An exposed trigger is `POST {mount}/actions/{name}` and `api.run<Name>()` in
//! the generated client ([`app_triggers`](crate::app_triggers)). Only a trigger
//! with **no event** (`none`) may be newly exposed: that is the kind that runs
//! only when something asks it to, and an insert or a daily trigger run from a
//! button would fire without the row or the clock its action expects. A trigger
//! already exposed is kept whatever it is now, so a list can always be shrunk —
//! including by removing a trigger that was deleted since.

use sc_action::{EventKind, ROLE_PUBLIC, Trigger};
use sc_api::op_name;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::application::{Application, TriggerRef};

/// What a call asks of the exposed list: `set` replaces it, then `add` and
/// `remove` change it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriggerChange {
    /// Names appended when not already exposed.
    pub add: Vec<String>,
    /// Names taken out.
    pub remove: Vec<String>,
    /// The whole new list, when given.
    pub set: Option<Vec<String>>,
}

/// Apply `change` to `app.triggers`, looking each newly exposed trigger up with
/// `require` (the live set's own, so a trigger that exists but is not usable is
/// refused with its reason). Returns the newly exposed triggers, so a caller
/// can check a grant against them. On an error `app` is unchanged.
pub fn change_app_triggers(
    app: &mut Application,
    change: &TriggerChange,
    require: impl Fn(&str) -> Result<Trigger>,
) -> Result<Vec<Trigger>> {
    for name in &change.remove {
        let in_set = change.set.as_ref().is_some_and(|s| s.contains(name));
        if !app.exposes_trigger(name) && !in_set {
            return Err(Error::invalid(format!(
                "application `{}` does not expose trigger `{name}`; it exposes {}. \
                 Nothing was changed.",
                app.subdomain,
                exposed_list(app)
            )));
        }
    }
    let mut names: Vec<String> = match &change.set {
        Some(set) => set.clone(),
        None => app.triggers.iter().map(|t| t.0.clone()).collect(),
    };
    for add in &change.add {
        if !names.contains(add) {
            names.push(add.clone());
        }
    }
    names.retain(|n| !change.remove.contains(n));

    let mut added = Vec::new();
    for name in &names {
        if app.exposes_trigger(name) {
            continue;
        }
        let trigger = require(name).map_err(|e| {
            Error::invalid(format!(
                "cannot expose trigger `{name}`: {e}. Nothing was changed."
            ))
        })?;
        if trigger.when != EventKind::None {
            return Err(Error::invalid(format!(
                "cannot expose trigger `{name}`: it runs on `{}`, and only a trigger \
                 with no event (`none`) can be called from an application. Save it \
                 with `when: \"none\"`, or make a separate one. Nothing was changed.",
                trigger.when.as_str()
            )));
        }
        added.push(trigger);
    }
    // Two names that project one client method would clash at mount time; say
    // so now, while it is still this call's to refuse.
    for (i, a) in names.iter().enumerate() {
        if let Some(b) = names[..i]
            .iter()
            .find(|b| op_name("run", b) == op_name("run", a))
        {
            return Err(Error::invalid(format!(
                "triggers `{b}` and `{a}` would both be `{}` in the client; expose one \
                 of them. Nothing was changed.",
                op_name("run", a)
            )));
        }
    }
    app.triggers = names.into_iter().map(TriggerRef).collect();
    Ok(added)
}

/// The newly exposed triggers anyone may call, signed out included — the ones
/// a caller needs a stronger grant for.
pub fn public_triggers(added: &[Trigger]) -> Vec<&str> {
    added
        .iter()
        .filter(|t| t.min_role == Some(ROLE_PUBLIC))
        .map(|t| t.name.as_str())
        .collect()
}

/// Each exposed trigger as an answer reports it: its action, who may call it,
/// the client method and the route. `lookup` finds it in the live set; one that
/// is not there is listed with the reason, so the list never hides it.
pub fn app_triggers_json(app: &Application, lookup: impl Fn(&str) -> Result<Trigger>) -> Json {
    let mount = app
        .apis
        .iter()
        .find(|a| a.provider == "rest")
        .map(|a| a.mount.trim_end_matches('/').to_owned());
    json!(
        app.triggers
            .iter()
            .map(|t| {
                let name = &t.0;
                let mut out = json!({
                    "name": name,
                    "method": op_name("run", name),
                    "route": mount
                        .as_ref()
                        .map(|m| format!("POST {m}/actions/{name}")),
                });
                match lookup(name) {
                    Ok(trigger) => {
                        out["action"] = json!(trigger.action().unwrap_or("(workflow)"));
                        out["min_role"] = json!(trigger.min_role);
                        if trigger.min_role.is_none() {
                            out["note"] = json!(
                                "min_role is not set, so only admins may call it; set \
                                 one with save_trigger (e.g. 80 for any signed-in user)"
                            );
                        }
                    }
                    Err(e) => out["note"] = json!(format!("not callable: {e}")),
                }
                out
            })
            .collect::<Vec<_>>()
    )
}

fn exposed_list(app: &Application) -> String {
    match app.triggers.is_empty() {
        true => "none".to_owned(),
        false => app
            .triggers
            .iter()
            .map(|t| format!("`{}`", t.0))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiConfig, FrameworkRef};

    fn live() -> Vec<Trigger> {
        vec![
            Trigger::new("mail_test_account", EventKind::None, "mail_test_account").min_role(80),
            Trigger::new("mail_send", EventKind::None, "mail_send"),
            Trigger::new("open_door", EventKind::None, "open_door").min_role(ROLE_PUBLIC),
            Trigger::new("nightly", EventKind::Daily, "cleanup"),
            Trigger::new("on_order", EventKind::Insert, "notify").on("orders"),
            Trigger::new("mail-send", EventKind::None, "mail_send"),
        ]
    }

    fn require(name: &str) -> Result<Trigger> {
        live()
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| Error::not_found(format!("no trigger `{name}`")))
    }

    fn app() -> Application {
        Application::new("Shop", "shop", FrameworkRef::new("react"))
            .with_api(ApiConfig::new("rest", "api"))
    }

    fn add(names: &[&str]) -> TriggerChange {
        TriggerChange {
            add: names.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn exposing_reports_method_route_and_role_and_notes_admin_only() {
        let mut app = app();
        let added =
            change_app_triggers(&mut app, &add(&["mail_test_account", "mail_send"]), require)
                .unwrap();
        assert_eq!(added.len(), 2);
        assert!(public_triggers(&added).is_empty());
        let out = app_triggers_json(&app, require);
        assert_eq!(out[0]["method"], "runMailTestAccount");
        assert_eq!(out[0]["route"], "POST /api/actions/mail_test_account");
        assert_eq!(out[0]["min_role"], 80);
        assert!(out[0].get("note").is_none());
        assert_eq!(out[1]["min_role"], Json::Null);
        assert!(out[1]["note"].as_str().unwrap().contains("only admins"));
    }

    #[test]
    fn a_missing_or_event_trigger_is_refused_by_name_and_nothing_changes() {
        let mut app = app();
        for (name, says) in [
            ("nope", "`nope`"),
            ("nightly", "`daily`"),
            ("on_order", "`insert`"),
        ] {
            let err = change_app_triggers(&mut app, &add(&["mail_send", name]), require)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("`{name}`")) && err.contains(says),
                "{err}"
            );
            assert!(app.triggers.is_empty());
        }
    }

    #[test]
    fn a_public_trigger_is_reported_and_a_clash_refused() {
        let mut app = app();
        let added = change_app_triggers(&mut app, &add(&["open_door"]), require).unwrap();
        assert_eq!(public_triggers(&added), vec!["open_door"]);
        // Already exposed: not added again, so no grant to check.
        let again = change_app_triggers(&mut app, &add(&["open_door"]), require).unwrap();
        assert!(again.is_empty());

        change_app_triggers(&mut app, &add(&["mail_send"]), require).unwrap();
        let err = change_app_triggers(&mut app, &add(&["mail-send"]), require)
            .unwrap_err()
            .to_string();
        assert!(err.contains("runMailSend"), "{err}");
    }

    #[test]
    fn removing_works_for_a_deleted_trigger_and_refuses_one_not_exposed() {
        let mut app = app();
        app.triggers = vec![TriggerRef("deleted_since".into())];
        let remove = |n: &str| TriggerChange {
            remove: vec![n.into()],
            ..Default::default()
        };
        assert!(change_app_triggers(&mut app, &remove("mail_send"), require).is_err());
        change_app_triggers(&mut app, &remove("deleted_since"), require).unwrap();
        assert!(app.triggers.is_empty());

        let set = TriggerChange {
            set: Some(vec!["mail_send".into(), "mail_test_account".into()]),
            ..Default::default()
        };
        change_app_triggers(&mut app, &set, require).unwrap();
        assert_eq!(app.triggers.len(), 2);
    }
}
