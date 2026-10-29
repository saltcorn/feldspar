//! Build hosts: other machines that build an application's targets for this
//! server (`TODO-build-hosts.md`, design §13.3 "Build hosts").
//!
//! iOS needs macOS and the server usually runs on Linux, so a Mac is registered
//! here as a build host for some targets. It runs the same binary as
//! `feldspar build-agent` and asks this server for work; the server never calls
//! it. **A build host never generates code, it only builds**: the server writes
//! the generated client and runtime from its own catalog before it hands a job
//! out, so a build host needs no database and cannot drift from the schema.
//!
//! This module is the registry and the agent's side of the conversation:
//!
//! - **`_fd_build_hosts`**: a name, the targets the host builds, the hash of its
//!   token, when it was last seen and what it last reported. The token is made
//!   and hashed as an API token is ([`sc_auth::new_bearer_secret`]): only the
//!   hash is stored, and the token is shown once, when the host is added.
//! - **The heartbeat**: the agent says who it is ([`AgentReport`]) and the
//!   server answers with the targets it builds and what each requires
//!   ([`heartbeat_answer`]), so the agent can check them on its own machine and
//!   report back. An agent speaking another [`BUILD_AGENT_PROTOCOL`] is refused,
//!   and the refusal is kept in its report, so the admin sees why it is idle.

use chrono::{DateTime, Duration, Utc};
use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Delete, Expr, Insert, Projection, Select, Source, Statement, Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::build::requirements_readiness;
use crate::declared::{TargetRequirement, installed_frameworks, requires_json, target_requires};

/// The build host table in the primary database.
pub const BUILD_HOSTS_TABLE: &str = "_fd_build_hosts";

/// What every build host token starts with: recognisable in a paste, and
/// never mistaken for an API token (`fspk_`).
pub const BUILD_HOST_TOKEN_PREFIX: &str = "fsbh_";

/// The environment variable an agent can read its token from. Build commands
/// never see it, so a build script or a package's install hook can't take it.
pub const BUILD_TOKEN_ENV: &str = "FELDSPAR_BUILD_TOKEN";

/// The version of the conversation between a build agent and this server. An
/// agent speaking another one is refused with the server's version to install.
pub const BUILD_AGENT_PROTOCOL: i64 = 1;

/// How often an agent sends its heartbeat, in seconds. The server answers it
/// with every heartbeat, so it can change without updating the agents.
pub const HEARTBEAT_SECONDS: i64 = 30;

/// A host not heard from for this long counts as offline: three heartbeats.
pub const ONLINE_SECONDS: i64 = 3 * HEARTBEAT_SECONDS;

const COL_ID: &str = "id";
const COL_NAME: &str = "name";
const COL_TARGETS: &str = "targets";
const COL_TOKEN_HASH: &str = "token_hash";
const COL_CREATED_AT: &str = "created_at";
const COL_LAST_SEEN_AT: &str = "last_seen_at";
const COL_REPORT: &str = "report";

fn build_host_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_TARGETS, json()).required(),
        DataField::plain(COL_TOKEN_HASH, text()).required().unique(),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        DataField::plain(COL_LAST_SEEN_AT, timestamp()),
        DataField::plain(COL_REPORT, json()),
    ]
}

/// Ensure the build host table exists. Idempotent, like every bootstrap.
pub async fn bootstrap_build_hosts(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(BUILD_HOSTS_TABLE, &build_host_fields())
        .await?;
    Ok(())
}

/// One registered build host, without its token's hash.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildHost {
    /// What a request names it by.
    pub id: Uuid,
    /// What the admin calls it: `Mac mini`.
    pub name: String,
    /// The targets it builds: `ios`, `ios_simulator`.
    pub targets: Vec<String>,
    /// When it was added.
    pub created_at: DateTime<Utc>,
    /// Its last heartbeat; `None` for a host whose agent never connected.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// What its agent last said ([`AgentReport`] as JSON, plus `refused` when
    /// the server turned it away).
    pub report: Option<Json>,
}

impl BuildHost {
    /// Whether its agent was heard from within [`ONLINE_SECONDS`].
    pub fn is_online(&self, now: DateTime<Utc>) -> bool {
        self.last_seen_at
            .is_some_and(|seen| now - seen < Duration::seconds(ONLINE_SECONDS))
            && self.refusal().is_none()
    }

    /// Why the server turned its agent away at the last heartbeat, if it did.
    pub fn refusal(&self) -> Option<&str> {
        self.report.as_ref()?.get("refused")?.as_str()
    }
}

/// Add a build host, answering it and its token: the one time the token
/// exists outside the agent that will hold it.
pub async fn add_build_host(
    catalog: &Catalog,
    name: &str,
    targets: &[String],
) -> Result<(BuildHost, String)> {
    let name = valid_name(name)?;
    let targets = valid_targets(targets)?;
    if find_by_name(catalog, &name).await?.is_some() {
        return Err(Error::invalid(format!(
            "there is already a build host called `{name}`"
        )));
    }
    let secret = sc_auth::new_bearer_secret(BUILD_HOST_TOKEN_PREFIX);
    let host = BuildHost {
        id: Uuid::new_v4(),
        name,
        targets,
        created_at: Utc::now(),
        last_seen_at: None,
        report: None,
    };
    let insert = Insert::row(
        BUILD_HOSTS_TABLE,
        [
            COL_ID,
            COL_NAME,
            COL_TARGETS,
            COL_TOKEN_HASH,
            COL_CREATED_AT,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::Lit(Value::Uuid(host.id)),
            Expr::Lit(Value::Text(host.name.clone())),
            Expr::Lit(Value::Json(json!(host.targets))),
            Expr::Lit(Value::Text(sc_auth::bearer_secret_hash(&secret))),
            Expr::Lit(Value::Timestamp(host.created_at)),
        ],
    );
    rows(catalog, Statement::from(insert)).await?;
    Ok((host, secret))
}

/// Every build host, by name.
pub async fn list_build_hosts(catalog: &Catalog) -> Result<Vec<BuildHost>> {
    let select = Select::from(Source::table(BUILD_HOSTS_TABLE));
    let mut hosts: Vec<BuildHost> = rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(host_from_row)
        .collect::<Result<_>>()?;
    hosts.sort_by_key(|h| h.name.to_lowercase());
    Ok(hosts)
}

/// Rename a build host or change its targets. `None` when there is no such host.
/// Its token stays valid.
pub async fn update_build_host(
    catalog: &Catalog,
    id: Uuid,
    name: &str,
    targets: &[String],
) -> Result<Option<BuildHost>> {
    let name = valid_name(name)?;
    let targets = valid_targets(targets)?;
    if let Some(other) = find_by_name(catalog, &name).await?
        && other.id != id
    {
        return Err(Error::invalid(format!(
            "there is already a build host called `{name}`"
        )));
    }
    let update = Update {
        returning: vec![Projection::all()],
        ..Update::new(
            BUILD_HOSTS_TABLE,
            vec![
                Assignment::new(COL_NAME.to_owned(), Expr::Lit(Value::Text(name))),
                Assignment::new(
                    COL_TARGETS.to_owned(),
                    Expr::Lit(Value::Json(json!(targets))),
                ),
            ],
        )
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
    };
    rows(catalog, Statement::from(update))
        .await?
        .first()
        .map(host_from_row)
        .transpose()
}

/// Remove a build host, answering whether there was one. Its token stops
/// working at once: this is how a host is revoked.
pub async fn delete_build_host(catalog: &Catalog, id: Uuid) -> Result<bool> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Delete::from(BUILD_HOSTS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id)))
    };
    Ok(!rows(catalog, Statement::from(delete)).await?.is_empty())
}

/// The build host a presented token belongs to, or a refusal saying why there
/// is none.
pub async fn authenticate_build_host(catalog: &Catalog, presented: &str) -> Result<BuildHost> {
    if !presented.starts_with(BUILD_HOST_TOKEN_PREFIX) {
        return Err(Error::auth(format!(
            "this is not a build host token: one begins with `{BUILD_HOST_TOKEN_PREFIX}`"
        )));
    }
    let select = Select::from(Source::table(BUILD_HOSTS_TABLE))
        .filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(sc_auth::bearer_secret_hash(presented))))
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(host_from_row)
        .transpose()?
        .ok_or_else(|| {
            Error::auth(
                "this build host token is not recognised: the host may have been removed. \
                 An administrator adds it again under Settings → Build hosts.",
            )
        })
}

/// Save the report a build host sent with its heartbeat, and note that it was
/// seen just now.
pub async fn record_heartbeat(catalog: &Catalog, id: Uuid, report: &Json) -> Result<()> {
    let update = Update::new(
        BUILD_HOSTS_TABLE,
        vec![
            Assignment::new(
                COL_LAST_SEEN_AT.to_owned(),
                Expr::Lit(Value::Timestamp(Utc::now())),
            ),
            Assignment::new(
                COL_REPORT.to_owned(),
                Expr::Lit(Value::Json(report.clone())),
            ),
        ],
    )
    .filter(Expr::col(COL_ID).eq(Expr::lit(id)));
    rows(catalog, Statement::from(update)).await?;
    Ok(())
}

// --- the heartbeat -------------------------------------------------------------

/// What an agent says at every heartbeat: who it is, and whether its machine
/// has what each of its targets requires. `readiness` is empty until the agent
/// has heard the requirements once.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReport {
    /// [`BUILD_AGENT_PROTOCOL`] as the agent knows it.
    pub protocol: i64,
    /// The agent's `feldspar` version.
    pub version: String,
    /// Its operating system and architecture, as Rust names them: `macos`, `aarch64`.
    pub os: String,
    pub arch: String,
    /// Per target: what is missing on the agent's machine (empty: ready).
    pub readiness: Vec<(String, Vec<String>)>,
}

impl AgentReport {
    /// This machine, with `readiness` checked against `targets` (an answer's
    /// targets, see [`heartbeat_answer`]).
    pub fn of_this_machine(targets: &[AnsweredTarget]) -> AgentReport {
        AgentReport {
            protocol: BUILD_AGENT_PROTOCOL,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            readiness: targets
                .iter()
                .map(|t| (t.name.clone(), requirements_readiness(&t.requires).missing))
                .collect(),
        }
    }

    pub fn to_json(&self) -> Json {
        let readiness: Map<String, Json> = self
            .readiness
            .iter()
            .map(|(target, missing)| (target.clone(), json!({ "missing": missing })))
            .collect();
        json!({
            "protocol": self.protocol,
            "version": self.version,
            "os": self.os,
            "arch": self.arch,
            "readiness": readiness,
        })
    }

    pub fn from_json(value: &Json) -> Result<AgentReport> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid(format!("a heartbeat needs `{key}`")))
        };
        let protocol = value
            .get("protocol")
            .and_then(Json::as_i64)
            .ok_or_else(|| Error::invalid("a heartbeat needs `protocol`"))?;
        let mut readiness = Vec::new();
        if let Some(Json::Object(map)) = value.get("readiness") {
            for (target, entry) in map {
                let missing = entry
                    .get("missing")
                    .and_then(Json::as_array)
                    .map(|m| {
                        m.iter()
                            .filter_map(Json::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                readiness.push((target.clone(), missing));
            }
        }
        Ok(AgentReport {
            protocol,
            version: text("version")?,
            os: text("os")?,
            arch: text("arch")?,
            readiness,
        })
    }
}

/// Refuse an agent that speaks another protocol, saying which `feldspar` to
/// install on it.
pub fn check_protocol(report: &AgentReport) -> Result<()> {
    if report.protocol == BUILD_AGENT_PROTOCOL {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "this build agent is feldspar {} (protocol {}), and this server needs protocol \
         {BUILD_AGENT_PROTOCOL}: install feldspar {} on the build host",
        report.version,
        report.protocol,
        env!("CARGO_PKG_VERSION"),
    )))
}

/// One target in a heartbeat's answer: what the agent checks on its machine.
#[derive(Debug, Clone, PartialEq)]
pub struct AnsweredTarget {
    /// The target's key: `ios`.
    pub name: String,
    /// What the admin UI calls it: `iOS app`.
    pub label: String,
    /// What its machine must have, from every framework that declares it.
    pub requires: Vec<TargetRequirement>,
}

/// The server's answer to a heartbeat from `host`: its name, the interval, and
/// each of its targets with what it requires, read from the installed
/// frameworks. A target no framework declares is answered with no
/// requirements and its own name as its label.
pub fn heartbeat_answer(host: &BuildHost) -> Json {
    let frameworks = installed_frameworks();
    let targets: Vec<Json> = host
        .targets
        .iter()
        .map(|name| {
            let declared: Vec<_> = frameworks
                .declarations()
                .iter()
                .flat_map(|decl| decl.targets.iter().filter(|t| &t.name == name))
                .collect();
            let label = declared.first().map_or(name.clone(), |t| t.label.clone());
            let mut requires: Vec<TargetRequirement> = Vec::new();
            for requirement in declared.iter().flat_map(|t| t.requires.iter()) {
                if !requires.contains(requirement) {
                    requires.push(requirement.clone());
                }
            }
            json!({ "name": name, "label": label, "requires": requires_json(&requires) })
        })
        .collect();
    json!({
        "name": host.name,
        "interval_seconds": HEARTBEAT_SECONDS,
        "targets": targets,
    })
}

/// The targets of a heartbeat's answer, as the agent reads them.
pub fn answered_targets(answer: &Json) -> Result<Vec<AnsweredTarget>> {
    let Some(targets) = answer.get("targets").and_then(Json::as_array) else {
        return Err(Error::invalid("the server's answer names no `targets`"));
    };
    targets
        .iter()
        .map(|t| {
            let name = t
                .get("name")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::invalid("a target in the server's answer has no `name`"))?;
            Ok(AnsweredTarget {
                name: name.to_owned(),
                label: t
                    .get("label")
                    .and_then(Json::as_str)
                    .unwrap_or(name)
                    .to_owned(),
                requires: target_requires(name, t.get("requires"))?,
            })
        })
        .collect()
}

// --- rows ----------------------------------------------------------------------

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid(
            "a build host needs a name: it is what the build button says it builds on",
        ));
    }
    Ok(name.to_owned())
}

fn valid_targets(targets: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for target in targets.iter().map(|t| t.trim()) {
        if !target.is_empty() && !out.iter().any(|t| t == target) {
            out.push(target.to_owned());
        }
    }
    if out.is_empty() {
        return Err(Error::invalid(
            "a build host needs at least one target to build, such as `ios`",
        ));
    }
    Ok(out)
}

async fn find_by_name(catalog: &Catalog, name: &str) -> Result<Option<BuildHost>> {
    Ok(list_build_hosts(catalog)
        .await?
        .into_iter()
        .find(|h| h.name.eq_ignore_ascii_case(name)))
}

fn host_from_row(row: &Row) -> Result<BuildHost> {
    let bad = |column: &str| Error::invalid(format!("{BUILD_HOSTS_TABLE}.{column} is malformed"));
    let timestamp = |column: &str| match row.get(column) {
        Some(Value::Timestamp(at)) => Ok(Some(*at)),
        Some(Value::Null) | None => Ok(None),
        _ => Err(bad(column)),
    };
    Ok(BuildHost {
        id: match row.get(COL_ID) {
            Some(Value::Uuid(id)) => *id,
            _ => return Err(bad(COL_ID)),
        },
        name: match row.get(COL_NAME) {
            Some(Value::Text(name)) => name.clone(),
            _ => return Err(bad(COL_NAME)),
        },
        targets: match row.get(COL_TARGETS) {
            Some(Value::Json(Json::Array(targets))) => targets
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect(),
            _ => return Err(bad(COL_TARGETS)),
        },
        created_at: timestamp(COL_CREATED_AT)?.ok_or_else(|| bad(COL_CREATED_AT))?,
        last_seen_at: timestamp(COL_LAST_SEEN_AT)?,
        report: match row.get(COL_REPORT) {
            Some(Value::Json(report)) if !report.is_null() => Some(report.clone()),
            _ => None,
        },
    })
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declared::TargetRequirementKind;

    fn host(targets: &[&str]) -> BuildHost {
        BuildHost {
            id: Uuid::new_v4(),
            name: "Mac mini".into(),
            targets: targets.iter().map(|t| (*t).to_owned()).collect(),
            created_at: Utc::now(),
            last_seen_at: None,
            report: None,
        }
    }

    #[test]
    fn a_report_goes_to_json_and_back() {
        let report = AgentReport {
            protocol: BUILD_AGENT_PROTOCOL,
            version: "0.1.0".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            readiness: vec![
                ("ios".into(), vec![]),
                ("android".into(), vec!["`ANDROID_HOME` is not set.".into()]),
            ],
        };
        let mut back = AgentReport::from_json(&report.to_json()).unwrap();
        back.readiness.sort();
        let mut expected = report.clone();
        expected.readiness.sort();
        assert_eq!(back, expected);
    }

    #[test]
    fn an_agent_on_another_protocol_is_told_which_feldspar_to_install() {
        let mut report = AgentReport::of_this_machine(&[]);
        assert!(check_protocol(&report).is_ok());
        report.protocol = BUILD_AGENT_PROTOCOL + 1;
        let why = check_protocol(&report).unwrap_err().to_string();
        assert!(
            why.contains(&format!("install feldspar {}", env!("CARGO_PKG_VERSION"))),
            "{why}"
        );
    }

    #[test]
    fn requirements_survive_the_answer_and_are_checked_on_the_agent() {
        let requires = vec![
            TargetRequirement {
                kind: TargetRequirementKind::Os {
                    name: "plan9".into(),
                },
                hint: "Use a Mac.".into(),
            },
            TargetRequirement {
                kind: TargetRequirementKind::Command {
                    name: "pod".into(),
                    dir_env: Some("FELDSPAR_POD_DIR".into()),
                },
                hint: String::new(),
            },
        ];
        let answer = json!({
            "name": "Mac mini",
            "targets": [{ "name": "ios", "label": "iOS app", "requires": requires_json(&requires) }],
        });
        let targets = answered_targets(&answer).unwrap();
        assert_eq!(targets[0].requires, requires);
        let report = AgentReport::of_this_machine(&targets);
        let missing = &report.readiness[0].1;
        assert!(
            missing[0].contains("only on plan9") && missing[0].ends_with("Use a Mac."),
            "{missing:?}"
        );
    }

    #[test]
    fn an_undeclared_target_is_answered_by_its_name() {
        let answer = heartbeat_answer(&host(&["no_such_target"]));
        assert_eq!(answer["name"], "Mac mini");
        assert_eq!(answer["interval_seconds"], HEARTBEAT_SECONDS);
        assert_eq!(
            answer["targets"],
            json!([{ "name": "no_such_target", "label": "no_such_target", "requires": [] }])
        );
    }

    #[test]
    fn a_host_is_online_after_a_recent_heartbeat_that_was_not_refused() {
        let now = Utc::now();
        let mut h = host(&["ios"]);
        assert!(!h.is_online(now));
        h.last_seen_at = Some(now - Duration::seconds(5));
        h.report = Some(json!({ "protocol": 1 }));
        assert!(h.is_online(now));
        h.report = Some(json!({ "refused": "too old" }));
        assert!(!h.is_online(now));
        assert_eq!(h.refusal(), Some("too old"));
        h.report = None;
        h.last_seen_at = Some(now - Duration::seconds(ONLINE_SECONDS + 1));
        assert!(!h.is_online(now));
    }

    #[test]
    fn names_and_targets_are_trimmed_and_required() {
        assert!(valid_name("  ").is_err());
        assert_eq!(valid_name(" Mac mini ").unwrap(), "Mac mini");
        assert!(valid_targets(&[" ".into()]).is_err());
        assert_eq!(
            valid_targets(&["ios".into(), " ios ".into(), "ios_simulator".into()]).unwrap(),
            vec!["ios", "ios_simulator"]
        );
    }
}
