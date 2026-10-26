//! The agents an application exposes to its own client (§11.7).
//!
//! An agent is server-side configuration, and like a trigger or a stream it
//! becomes reachable from an application's pages only because the application
//! says so: [`Application::agents`] is an **opt-in subset**, and an agent the
//! application does not name has no route — a request for it is a 404, not a
//! 403.
//!
//! An exposed agent is served beside the endpoint set, under the application's
//! first API mount (as the live socket and the upload slots are), at
//! `{mount}/_agents/{name}/…` ([`agent_route`]):
//!
//! | Route | What it is |
//! |---|---|
//! | `POST …/chat` | one chat turn, streamed back as server-sent events |
//! | `POST …/prompt` | one prompt, answered as JSON (or streamed with `?stream=1`) |
//! | `GET …/runs` | the caller's own conversations with it, without transcripts |
//! | `GET …/runs/{id}` | one of them, with its transcript |
//! | `PATCH …/runs/{id}` | rename one |
//! | `DELETE …/runs/{id}` | delete one |
//!
//! Who may use it is the **agent's** `min_role`, the one floor on who may talk
//! to an agent wherever it is reached from. What the exposure adds is a second
//! opt-in for people who are not signed in ([`AgentExposure::anonymous`]): an
//! agent whose floor is public is still refused to an anonymous caller unless
//! the application also says it may be used signed out.

use std::collections::BTreeMap;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_query::{Select, Source, Statement};
use serde_json::{Map, Value as Json, json};

use crate::application::{Application, percent_decode, percent_encode};

/// The path segment exposed agents are served under, beside the endpoint set.
/// Underscored, as `_upload` is, so it cannot be a table's REST path.
pub const AGENTS_SEGMENT: &str = "_agents";

/// The table the agents are stored in. `sc-agent` owns it and sits above this
/// crate, so the name is repeated here for the one read this crate makes of it:
/// whether an exposed agent exists and who may use it ([`check_exposed_agents`]).
const AGENTS_TABLE: &str = "_fd_agents";

/// How much of what an agent's tools did an application's client is shown, in
/// the streamed events and in a stored transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolDetail {
    /// Nothing: the person sees the agent's words and no sign of its tools.
    None,
    /// That a tool ran, which one, and whether it failed — enough for a
    /// "Searching the catalogue…" line. The default.
    #[default]
    Names,
    /// The tool's arguments and its result as well. A result is whatever the
    /// tool returned to the model, read with the caller's own authority, so
    /// this shows the person nothing they could not read themselves; it is
    /// still more than most pages want to draw.
    Full,
}

impl ToolDetail {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolDetail::None => "none",
            ToolDetail::Names => "names",
            ToolDetail::Full => "full",
        }
    }

    /// Read the stored spelling back.
    pub fn parse(s: &str) -> Result<ToolDetail> {
        match s {
            "none" => Ok(ToolDetail::None),
            "names" => Ok(ToolDetail::Names),
            "full" => Ok(ToolDetail::Full),
            other => Err(Error::invalid(format!(
                "an exposed agent's `tools` should be `none`, `names` or `full`, got `{other}`"
            ))),
        }
    }
}

/// One agent an application exposes to its client (§11.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExposure {
    /// The agent's name, as `_fd_agents.name` and `_fd_runs.subject` hold it.
    pub name: String,
    /// Whether the client may hold conversations with it: `…/chat` and the
    /// `…/runs` history.
    pub chat: bool,
    /// Whether the client may send it a single prompt: `…/prompt`.
    pub prompt: bool,
    /// Whether people who are **not signed in** may use it. Off by default, and
    /// on its own not enough: the agent's `min_role` must also be public (100),
    /// so making an agent anonymous takes both the agent's owner and the
    /// application's.
    pub anonymous: bool,
    /// How much of what the agent's tools did the client is shown.
    pub tools: ToolDetail,
    /// A template the `…/prompt` route fills from the request's `input` object,
    /// with `{{key}}` for each value, instead of accepting the prompt as free
    /// text. An application that wants "summarise order {{order}}" from a button
    /// does not then hand every visitor a free-form line to the agent.
    pub prompt_template: Option<String>,
}

impl AgentExposure {
    /// Expose `name` for chat and prompts, to signed-in callers, showing which
    /// tools ran.
    pub fn new(name: impl Into<String>) -> AgentExposure {
        AgentExposure {
            name: name.into(),
            chat: true,
            prompt: true,
            anonymous: false,
            tools: ToolDetail::default(),
            prompt_template: None,
        }
    }

    /// Set whether it may be chatted with.
    pub fn chat(mut self, chat: bool) -> AgentExposure {
        self.chat = chat;
        self
    }

    /// Set whether it may be sent a single prompt.
    pub fn prompt(mut self, prompt: bool) -> AgentExposure {
        self.prompt = prompt;
        self
    }

    /// Set whether people who are not signed in may use it.
    pub fn anonymous(mut self, anonymous: bool) -> AgentExposure {
        self.anonymous = anonymous;
        self
    }

    /// Set how much of its tools' work the client sees.
    pub fn tools(mut self, tools: ToolDetail) -> AgentExposure {
        self.tools = tools;
        self
    }

    /// Fill `…/prompt` from this template rather than taking free text.
    pub fn prompt_template(mut self, template: impl Into<String>) -> AgentExposure {
        self.prompt_template = Some(template.into());
        self
    }

    /// The exposure as the record, the admin API and a backup spell it.
    pub fn to_json(&self) -> Json {
        json!({
            "name": self.name,
            "chat": self.chat,
            "prompt": self.prompt,
            "anonymous": self.anonymous,
            "tools": self.tools.as_str(),
            "prompt_template": self.prompt_template,
        })
    }

    /// Read an exposure back: `name` is required, everything else has the
    /// default [`AgentExposure::new`] gives it. An unknown key is refused, as an
    /// upload slot's is, because a misspelled `anonymus: false` that was
    /// silently ignored would be read by its author as a decision.
    pub fn from_json(value: &Json) -> Result<AgentExposure> {
        let Json::Object(obj) = value else {
            return Err(Error::invalid(format!(
                "an exposed agent should be an object, got {value}"
            )));
        };
        const KEYS: [&str; 6] = [
            "name",
            "chat",
            "prompt",
            "anonymous",
            "tools",
            "prompt_template",
        ];
        if let Some(unknown) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(Error::invalid(format!(
                "an exposed agent has no setting `{unknown}`; the settings are {}",
                KEYS.join(", ")
            )));
        }
        let flag = |key: &str, default: bool| -> Result<bool> {
            match obj.get(key) {
                None | Some(Json::Null) => Ok(default),
                Some(Json::Bool(b)) => Ok(*b),
                Some(other) => Err(Error::invalid(format!(
                    "an exposed agent's `{key}` should be true or false, got {other}"
                ))),
            }
        };
        let name = match obj.get("name") {
            Some(Json::String(s)) => s.trim().to_owned(),
            None | Some(Json::Null) => {
                return Err(Error::invalid("an exposed agent needs a `name`"));
            }
            Some(other) => {
                return Err(Error::invalid(format!(
                    "an exposed agent's `name` should be text, got {other}"
                )));
            }
        };
        let tools = match obj.get("tools") {
            None | Some(Json::Null) => ToolDetail::default(),
            Some(Json::String(s)) => ToolDetail::parse(s.trim())?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "an exposed agent's `tools` should be `none`, `names` or `full`, got {other}"
                )));
            }
        };
        let prompt_template = match obj.get("prompt_template") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) if s.trim().is_empty() => None,
            Some(Json::String(s)) => Some(s.clone()),
            Some(other) => {
                return Err(Error::invalid(format!(
                    "an exposed agent's `prompt_template` should be text, got {other}"
                )));
            }
        };
        Ok(AgentExposure {
            name,
            chat: flag("chat", true)?,
            prompt: flag("prompt", true)?,
            anonymous: flag("anonymous", false)?,
            tools,
            prompt_template,
        })
    }

    /// The `{{key}}` placeholders of the prompt template, in order of first
    /// appearance; empty when there is no template.
    pub fn template_keys(&self) -> Vec<String> {
        self.prompt_template
            .as_deref()
            .map(template_keys)
            .unwrap_or_default()
    }

    /// The prompt a `…/prompt` request asks for: its free text, or the
    /// template filled from its `input`.
    ///
    /// With a template, `input` must give every placeholder a value and nothing
    /// else: a missing key would put a literal `{{order}}` in front of the
    /// model, and an extra one is a client that believes it is saying something
    /// the agent never hears. A string is inserted as it is, anything else as
    /// its JSON text.
    pub fn prompt_text(&self, prompt: Option<&str>, input: Option<&Json>) -> Result<String> {
        let text = match &self.prompt_template {
            None => {
                if input.is_some_and(|i| !i.is_null()) {
                    return Err(Error::invalid(format!(
                        "agent `{}` takes a free-text `prompt`, not an `input` object",
                        self.name
                    )));
                }
                prompt.unwrap_or("").to_owned()
            }
            Some(template) => {
                if prompt.is_some() {
                    return Err(Error::invalid(format!(
                        "agent `{}` is prompted from a template: send its values as `input` \
                         ({}), not a `prompt`",
                        self.name,
                        quoted(&self.template_keys())
                    )));
                }
                let empty = Map::new();
                let values = match input {
                    None | Some(Json::Null) => &empty,
                    Some(Json::Object(values)) => values,
                    Some(other) => {
                        return Err(Error::invalid(format!(
                            "`input` should be an object of {}, got {other}",
                            quoted(&self.template_keys())
                        )));
                    }
                };
                fill_template(template, values)?
            }
        };
        if text.trim().is_empty() {
            return Err(Error::invalid(format!(
                "there is nothing to ask agent `{}`: the prompt is empty",
                self.name
            )));
        }
        Ok(text)
    }
}

/// The placeholders in `template`, each once, in order.
fn template_keys(template: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let key = after[..close].trim().to_owned();
        if !keys.contains(&key) {
            keys.push(key);
        }
        rest = &after[close + 2..];
    }
    keys
}

/// `template` with every `{{key}}` replaced by its value in `values`.
fn fill_template(template: &str, values: &Map<String, Json>) -> Result<String> {
    let keys = template_keys(template);
    if let Some(extra) = values.keys().find(|k| !keys.contains(k)) {
        return Err(Error::invalid(format!(
            "`input` has `{extra}`, which the prompt template does not use; it takes {}",
            quoted(&keys)
        )));
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        out.push_str(&rest[..open]);
        let key = after[..close].trim();
        match values.get(key) {
            None | Some(Json::Null) => {
                return Err(Error::invalid(format!(
                    "`input` needs a value for `{key}`; the prompt template takes {}",
                    quoted(&keys)
                )));
            }
            Some(Json::String(s)) => out.push_str(s),
            Some(other) => out.push_str(&other.to_string()),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `a`, `b` and `c` — or `nothing`.
fn quoted(names: &[String]) -> String {
    if names.is_empty() {
        return "nothing".to_owned();
    }
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The exposures of a stored or submitted list, in order.
pub fn agent_exposures_from_json(value: Option<&Json>) -> Result<Vec<AgentExposure>> {
    match value {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items.iter().map(AgentExposure::from_json).collect(),
        Some(other) => Err(Error::invalid(format!(
            "`agents` should be a list of exposed agents, got {other}"
        ))),
    }
}

/// The exposures as a JSON list.
pub fn agent_exposures_json(exposures: &[AgentExposure]) -> Json {
    Json::Array(exposures.iter().map(AgentExposure::to_json).collect())
}

/// Check an application's exposed agents on save: each named, once, exposing
/// something, and with a template whose placeholders are names.
///
/// Whether each agent **exists**, and whether an anonymous one is public, is
/// [`check_exposed_agents`]' — it reads the agents, which this check does not,
/// so that a backup restoring an application before its agents still loads.
pub fn validate_agent_exposures(app: &Application) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for exposure in &app.agents {
        let name = &exposure.name;
        if name.is_empty() {
            return Err(Error::invalid(format!(
                "application `{}` exposes an agent with no name",
                app.name
            )));
        }
        if !seen.insert(name.as_str()) {
            return Err(Error::invalid(format!(
                "application `{}` exposes agent `{name}` twice",
                app.name
            )));
        }
        if !exposure.chat && !exposure.prompt {
            return Err(Error::invalid(format!(
                "exposed agent `{name}` allows neither chat nor prompts, so it exposes \
                 nothing; turn one of them on, or stop exposing it"
            )));
        }
        if let Some(template) = &exposure.prompt_template {
            let keys = template_keys(template);
            if keys.is_empty() {
                return Err(Error::invalid(format!(
                    "exposed agent `{name}`'s prompt template has no `{{{{key}}}}` \
                     placeholder, so every prompt would be the same; leave the template \
                     empty to take free text"
                )));
            }
            if let Some(bad) = keys.iter().find(|k| !is_identifier(k)) {
                return Err(Error::invalid(format!(
                    "exposed agent `{name}`'s prompt template has a placeholder `{{{{{bad}}}}}`; \
                     a placeholder is a name (letters, digits and `_`), since it is a key of \
                     the `input` object"
                )));
            }
        }
    }
    Ok(())
}

/// Whether `s` is a name: letters, digits and `_`, not starting with a digit.
fn is_identifier(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check the exposed agents against the agents there are: each must exist, and
/// one exposed to anonymous callers must have the public floor.
///
/// Made where a person changes an application (the admin API, the MCP tool),
/// so the mistake is refused in front of whoever made it. A request for an
/// agent deleted since is refused at request time with the same reason.
pub async fn check_exposed_agents(catalog: &Catalog, app: &Application) -> Result<()> {
    if app.agents.is_empty() {
        return Ok(());
    }
    let floors = stored_agent_floors(catalog).await?;
    for exposure in &app.agents {
        let Some(floor) = floors.get(&exposure.name) else {
            let names: Vec<String> = floors.keys().cloned().collect();
            return Err(Error::invalid(format!(
                "application `{}` exposes agent `{}`, and there is no agent of that name; \
                 the agents are {}",
                app.name,
                exposure.name,
                quoted(&names)
            )));
        };
        if exposure.anonymous && *floor != Some(100) {
            return Err(Error::invalid(format!(
                "agent `{}` is exposed to people who are not signed in, but its own minimum \
                 role is {}; set the agent's minimum role to public (100) as well, or turn \
                 `anonymous` off",
                exposure.name,
                match floor {
                    Some(role) => role.to_string(),
                    None => "admin-only".to_owned(),
                }
            )));
        }
    }
    Ok(())
}

/// The names of the applications that expose agent `name`: what stands in the
/// way of deleting or renaming it, since an exposed agent that is gone is a
/// page whose chat stopped working.
pub async fn applications_exposing_agent(catalog: &Catalog, name: &str) -> Result<Vec<String>> {
    let mut names: Vec<String> = crate::list_applications(catalog)
        .await?
        .into_iter()
        .filter(|app| app.agents.iter().any(|a| a.name == name))
        .map(|app| app.name)
        .collect();
    names.sort();
    Ok(names)
}

/// Every stored agent's name and `min_role`.
async fn stored_agent_floors(catalog: &Catalog) -> Result<BTreeMap<String, Option<u8>>> {
    let select = Select::from(Source::table(AGENTS_TABLE));
    let rows: Vec<sc_db::Row> = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name = match row.get("name") {
                Some(sc_query::Value::Text(name)) => name.clone(),
                _ => return None,
            };
            let floor = match row.get("min_role") {
                Some(sc_query::Value::Int(role)) => u8::try_from(*role).ok(),
                _ => None,
            };
            Some((name, floor))
        })
        .collect())
}

/// The application's mount: its first API's, as the live socket's and the
/// upload slots' are. Empty for an application with no API provider.
fn mount(app: &Application) -> &str {
    app.apis
        .first()
        .map(|api| api.mount.trim_end_matches('/'))
        .unwrap_or("")
}

/// Where the exposed agent `name` is served: `{mount}/_agents/{name}`, the name
/// percent-encoded. The generated client and the router both go through this.
pub fn agent_path(app: &Application, name: &str) -> String {
    format!("{}/{AGENTS_SEGMENT}/{}", mount(app), percent_encode(name))
}

/// Which of an exposed agent's routes a request is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRoute {
    /// `…/chat`: one streamed chat turn.
    Chat,
    /// `…/prompt`: one prompt.
    Prompt,
    /// `…/runs`: the caller's conversations.
    Runs,
    /// `…/runs/{id}`: one of them. The id is as it arrived; an id that is not
    /// one is the same 404 as a run that is somebody else's.
    Run(String),
    /// `…/controls`: the controls the agent adds to a chat's input row
    /// (TODO-agent-skills AS6.4).
    Controls,
    /// `…/runs/{id}/parts/{part}`: something a tool showed the person in that
    /// conversation (AS6.3).
    Part(String, String),
}

/// A request path that is one of `app`'s exposed agents' routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// The exposure, as the application declares it.
    pub exposure: AgentExposure,
    /// Which route.
    pub route: AgentRoute,
}

/// The exposed-agent route `path` addresses on `app`, if any.
///
/// A path under `{mount}/_agents/` naming no exposed agent, or no route of
/// one, is `None` and falls through to the providers, whose answer is the 404
/// any unknown path gets: an agent the application does not expose has nothing
/// there.
pub fn agent_route(app: &Application, path: &str) -> Option<AgentRequest> {
    if app.agents.is_empty() {
        return None;
    }
    let rest = path
        .strip_prefix(mount(app))?
        .strip_prefix('/')?
        .strip_prefix(AGENTS_SEGMENT)?
        .strip_prefix('/')?;
    let mut parts = rest.split('/');
    let name = percent_decode(parts.next()?);
    let segments: Vec<&str> = parts.collect();
    let route = match segments.as_slice() {
        ["chat"] => AgentRoute::Chat,
        ["prompt"] => AgentRoute::Prompt,
        ["controls"] => AgentRoute::Controls,
        ["runs"] => AgentRoute::Runs,
        ["runs", id] if !id.is_empty() => AgentRoute::Run((*id).to_owned()),
        ["runs", id, "parts", part] if !id.is_empty() && !part.is_empty() => {
            AgentRoute::Part((*id).to_owned(), (*part).to_owned())
        }
        _ => return None,
    };
    let exposure = app.agents.iter().find(|a| a.name == name)?.clone();
    Some(AgentRequest { exposure, route })
}

/// The application's exposed agents as its generated client names them —
/// `api.agents["<name>"]` (§11.7).
pub fn agent_exports(app: &Application) -> Vec<sc_api::AgentExport> {
    app.agents
        .iter()
        .map(|exposure| sc_api::AgentExport {
            name: exposure.name.clone(),
            path: agent_path(app, &exposure.name),
            chat: exposure.chat,
            prompt: exposure.prompt,
            template_keys: exposure.template_keys(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiConfig, FrameworkRef};

    fn app() -> Application {
        Application::new("Help", "help", FrameworkRef::new("code"))
            .with_api(ApiConfig::new("rest", "/api"))
            .with_agent(AgentExposure::new("helpdesk"))
            .with_agent(
                AgentExposure::new("order summary")
                    .chat(false)
                    .prompt_template("Summarise order {{order}} for {{ audience }}."),
            )
    }

    #[test]
    fn an_exposure_round_trips_and_refuses_a_misspelled_setting() {
        let exposure = AgentExposure::new("helpdesk")
            .anonymous(true)
            .tools(ToolDetail::Full)
            .prompt_template("Say {{x}}");
        assert_eq!(
            AgentExposure::from_json(&exposure.to_json()).unwrap(),
            exposure
        );
        // Only the name is required, and the defaults are the safe ones.
        let bare = AgentExposure::from_json(&json!({ "name": "helpdesk" })).unwrap();
        assert_eq!(bare, AgentExposure::new("helpdesk"));
        assert!(!bare.anonymous);
        assert_eq!(bare.tools, ToolDetail::Names);
        let err = AgentExposure::from_json(&json!({ "name": "a", "anonymus": true }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("anonymus"), "{err}");
        assert!(AgentExposure::from_json(&json!({ "name": "a", "tools": "all" })).is_err());
    }

    #[test]
    fn routes_resolve_under_the_mount_and_only_for_exposed_agents() {
        let app = app();
        assert_eq!(
            agent_path(&app, "order summary"),
            "/api/_agents/order%20summary"
        );
        let at = |p: &str| agent_route(&app, p).map(|r| (r.exposure.name, r.route));
        assert_eq!(
            at("/api/_agents/helpdesk/chat"),
            Some(("helpdesk".to_owned(), AgentRoute::Chat))
        );
        assert_eq!(
            at("/api/_agents/order%20summary/prompt"),
            Some(("order summary".to_owned(), AgentRoute::Prompt))
        );
        assert_eq!(
            at("/api/_agents/helpdesk/runs"),
            Some(("helpdesk".to_owned(), AgentRoute::Runs))
        );
        assert_eq!(
            at("/api/_agents/helpdesk/runs/abc"),
            Some(("helpdesk".to_owned(), AgentRoute::Run("abc".to_owned())))
        );
        // An agent that is not exposed, a route that does not exist, and a
        // path outside the mount are all nothing.
        assert_eq!(at("/api/_agents/copilot/chat"), None);
        assert_eq!(at("/api/_agents/helpdesk/delete"), None);
        assert_eq!(at("/api/_agents/helpdesk/runs/a/b"), None);
        assert_eq!(
            at("/api/_agents/helpdesk/controls"),
            Some(("helpdesk".to_owned(), AgentRoute::Controls))
        );
        assert_eq!(
            at("/api/_agents/helpdesk/runs/r/parts/p"),
            Some((
                "helpdesk".to_owned(),
                AgentRoute::Part("r".to_owned(), "p".to_owned())
            ))
        );
        assert_eq!(at("/other/_agents/helpdesk/chat"), None);
    }

    #[test]
    fn a_template_is_filled_from_exactly_its_keys() {
        let exposure = app().agents[1].clone();
        assert_eq!(exposure.template_keys(), vec!["order", "audience"]);
        assert_eq!(
            exposure
                .prompt_text(
                    None,
                    Some(&json!({ "order": 42, "audience": "the customer" }))
                )
                .unwrap(),
            "Summarise order 42 for the customer."
        );
        let missing = exposure
            .prompt_text(None, Some(&json!({ "order": 42 })))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("audience"), "{missing}");
        let extra = exposure
            .prompt_text(
                None,
                Some(&json!({ "order": 1, "audience": "x", "note": "y" })),
            )
            .unwrap_err()
            .to_string();
        assert!(extra.contains("note"), "{extra}");
        // Free text is refused where a template is the only way in.
        assert!(exposure.prompt_text(Some("anything"), None).is_err());
    }

    #[test]
    fn free_text_must_say_something() {
        let exposure = AgentExposure::new("helpdesk");
        assert_eq!(exposure.prompt_text(Some("hi"), None).unwrap(), "hi");
        assert!(exposure.prompt_text(Some("   "), None).is_err());
        assert!(exposure.prompt_text(None, None).is_err());
        assert!(
            exposure
                .prompt_text(Some("hi"), Some(&json!({ "x": 1 })))
                .is_err()
        );
    }

    #[test]
    fn validation_refuses_duplicates_an_empty_exposure_and_a_bad_template() {
        validate_agent_exposures(&app()).unwrap();
        let twice = app().with_agent(AgentExposure::new("helpdesk"));
        assert!(validate_agent_exposures(&twice).is_err());
        let nothing = app().with_agent(AgentExposure::new("x").chat(false).prompt(false));
        assert!(validate_agent_exposures(&nothing).is_err());
        let constant = app().with_agent(AgentExposure::new("y").prompt_template("no keys"));
        assert!(validate_agent_exposures(&constant).is_err());
        let spaced = app().with_agent(AgentExposure::new("z").prompt_template("{{a b}}"));
        assert!(validate_agent_exposures(&spaced).is_err());
    }
}
