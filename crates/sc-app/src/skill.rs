//! The generated `SKILL.md`: which half of an application a coding agent is
//! looking at, and the tools that reach the other half (design §13.6, GOALS
//! "generate a SKILL.md file if they want to use an external coding agent").
//!
//! An application is half **code in this repository** and half **configuration
//! in the server's database**. A coding agent has the first half through the
//! filesystem and can see none of the second: there is no file under the project
//! that says the `tasks` table has a `priority` column, or that a trigger fires
//! when it changes. The administration MCP server is how it reaches that half —
//! and an agent that does not know the server exists will instead do the only
//! thing it can, which is to write front-end code against a schema it cannot
//! change and report the feature as done.
//!
//! So this file is written **beside the generated client**, on the same
//! schedule and for the same reason: the client is the code half's view of the
//! API, and this is the whole application's map. It names the tools the server
//! offers and — as loudly — the ones it does not, because "there is no tool for
//! writing this file, the repository is where that lives" is the sentence that
//! keeps the two halves from being edited through each other.
//!
//! ## Everything in it is derived
//!
//! The tool list is read from the same two places the server projects it from:
//! [`crate::mcp::tool_set`] for the composite tools and the
//! [`McpTag`](sc_api::McpTag)s of [`admin_endpoints`](sc_api::admin_endpoints)
//! for the generated ones. A tool added, renamed or tagged tomorrow is in the
//! next SKILL.md with nobody editing prose, which is the property a
//! hand-written list of thirty tool names cannot have.
//!
//! What is *not* derived is the one-line summary: it is the **first clause** of
//! the description the model is given, cut at the first sentence, colon or dash
//! ([`first_clause`]). Restating each tool in a second sentence of its own would
//! be a second description to keep in step, and the real one arrives with the
//! tool anyway — this file is the map, not the manual.
//!
//! The listing is generated with **every** grant and both areas, because a
//! repository is not a token: the same project may be worked on with two tokens
//! granted different things. The tools a given token is actually offered are the
//! ones in its `tools/list`, and the file says so.

use sc_api::mcp::{Area, Areas, Projection};
use sc_api::schema_edit::Grants;
use sc_catalog::Catalog;

use crate::Application;

/// The file name, beside the generated client.
pub const SKILL_FILE: &str = "SKILL.md";

/// How much of a tool's first clause to keep before it stops being a label.
const SUMMARY_CHARS: usize = 180;

/// One line of the tool listing.
struct ToolLine {
    name: String,
    summary: String,
    /// The grant a caller needs before this one runs, where the tool declares
    /// one as data. The composite tools check theirs inside their own bodies —
    /// `edit_schema` needs a different grant per operation — so they carry none
    /// here, and the prose above the listing covers them.
    grant: Option<&'static str>,
}

impl ToolLine {
    fn render(&self) -> String {
        match self.grant {
            Some(key) => format!("- `{}` — {} (needs `{key}`)\n", self.name, self.summary),
            None => format!("- `{}` — {}\n", self.name, self.summary),
        }
    }
}

/// The whole administrative surface, split by the area that governs it.
///
/// Composite tools first and generated ones after, each in the order the server
/// lists them, so a reader comparing this against a `tools/list` sees the same
/// order.
fn tool_lines(catalog: &Catalog) -> (Vec<ToolLine>, Vec<ToolLine>, Vec<ToolLine>) {
    let grants = Grants::all();
    let set = crate::mcp::tool_set(grants, Areas::all());
    let mut always = Vec::new();
    let mut triggers = Vec::new();
    let mut applications = Vec::new();

    let mut push = |area: Option<Area>, line: ToolLine| match area {
        None => always.push(line),
        Some(Area::Triggers) => triggers.push(line),
        Some(Area::Applications) => applications.push(line),
    };

    for tool in set.tools() {
        push(
            tool.area(),
            ToolLine {
                name: tool.name().to_owned(),
                summary: first_clause(&tool.description(catalog, &grants)),
                grant: None,
            },
        );
    }
    let admin = sc_api::admin_endpoints();
    for projection in Projection::all(&admin) {
        push(
            projection.area(),
            ToolLine {
                name: projection.name().to_owned(),
                summary: first_clause(projection.description()),
                grant: projection.grant().map(|grant| grant.key()),
            },
        );
    }
    (always, triggers, applications)
}

/// The first clause of a tool's description, as a label.
///
/// Cut at whichever comes first of a sentence end, a colon or a spaced em dash —
/// which is not a trick, it is how these descriptions are written: they open
/// with what the tool does and then qualify it. "Describe the database schema:
/// every table with its label…" becomes "Describe the database schema", and
/// "Build an application and mount it live: regenerates…" keeps the half a
/// reader is scanning for.
fn first_clause(text: &str) -> String {
    let end = [". ", ".\n", ": ", ":\n", " — "]
        .iter()
        .filter_map(|pattern| text.find(pattern))
        .min()
        .unwrap_or(text.len());
    let clause = text[..end].trim().trim_end_matches('.');
    if clause.chars().count() <= SUMMARY_CHARS {
        return clause.to_owned();
    }
    // A description whose first clause is a paragraph: keep a whole number of
    // words of it rather than cutting a word — or a markdown span — in half.
    let cut = clause
        .char_indices()
        .nth(SUMMARY_CHARS)
        .map_or(clause.len(), |(idx, _)| idx);
    let kept = clause[..cut]
        .rsplit_once(' ')
        .map_or(clause, |(head, _)| head);
    format!("{kept}…")
}

/// What the skill calls itself in its front matter: a lower-case, hyphenated
/// name derived from the application.
///
/// The subdomain's first label where there is one — it is already a DNS label,
/// so it is already legal here — and the display name reduced to one otherwise,
/// since `Application::name` is `"My Blog"` and a skill name may not be.
fn skill_name(app: &Application) -> String {
    let subdomain = app.subdomain.split('.').next().unwrap_or_default().trim();
    let source = match subdomain.is_empty() {
        true => app.name.as_str(),
        false => subdomain,
    };
    let mut slug = String::new();
    for ch in source.chars() {
        match ch.is_ascii_alphanumeric() {
            true => slug.push(ch.to_ascii_lowercase()),
            false if !slug.ends_with('-') => slug.push('-'),
            false => {}
        }
    }
    match slug.trim_matches('-') {
        "" => "saltcorn-app".to_owned(),
        name => format!("saltcorn-{name}"),
    }
}

/// The `SKILL.md` for `app`, to be written beside its generated client.
///
/// `client_file` is the client's own file name, because the two sit in one
/// directory and the file's whole subject is which files are whose.
pub fn generate_skill(catalog: &Catalog, app: &Application, client_file: &str) -> String {
    let (always, triggers, applications) = tool_lines(catalog);
    let render = |lines: &[ToolLine]| lines.iter().map(ToolLine::render).collect::<String>();
    let subdomain = match app.subdomain.is_empty() {
        true => "no subdomain yet".to_owned(),
        false => format!("`{}`", app.subdomain),
    };
    format!(
        "---\n\
         name: {skill}\n\
         description: >-\n  \
           Working on the Saltcorn application `{name}`: which half of it lives in \
           this repository and which half lives in the Saltcorn server, and the \
           administration MCP tools that reach the second half. Read this before \
           adding a field, a trigger, a workflow or an API query.\n\
         ---\n\
         \n\
         # The Saltcorn application `{name}`\n\
         \n\
         *Generated by the Saltcorn server beside `{client_file}`, and rewritten \
         whenever that is. Do not edit it; copy it to \
         `.claude/skills/{skill}/{SKILL_FILE}` if you want it loaded \
         automatically.*\n\
         \n\
         This repository is **half** of the application `{name}`, served at \
         {subdomain}. The other half is configuration held by the Saltcorn \
         server, and it is not a file anywhere in this project:\n\
         \n\
         - **The code** is here. Edit it, then build it.\n\
         - **The tables and their fields, the access rules, the triggers and the \
         actions they run, the workflows, the agents, and the applications with \
         their custom SQL queries** are in the server's database. Nothing in this \
         repository can change them; the administration MCP tools below are how \
         they are reached.\n\
         \n\
         `{client_file}` and the files beside it are **generated from the second \
         half**: the server writes them from the tables and endpoints this \
         application exposes, and overwrites them on every build. A column that \
         is not in the database is not in the client, and adding it to the client \
         by hand does not add it to the database — it makes a file that the next \
         build deletes.\n\
         \n\
         ## Every string a person reads goes through `t()`\n\
         \n\
         The catalogue is the third generated thing, and the one you are \
         expected to feed: `feldspar i18n extract` reads the `t(\"…\")` call \
         sites out of this repository, the admin translates them on the \
         application's Translations screen, and the server serves the result. \
         A user-visible literal that no `t()` wraps is invisible to all three \
         and can never be translated, which is why `feldspar i18n lint` \
         reports it.\n\
         \n\
         The message id **is the English source text** (`t(\"Add a task\")`, not \
         `t(\"tasks.add\")`), the first argument must be a **string literal**, \
         and placeholders are `{{name}}` rather than a template. The project's \
         `AGENTS.md` has the call shapes.\n\
         \n\
         **Translating** is done with the tools, not by editing files: \
         `describe_translations` shows the strings and what each locale is \
         missing, the `locales` section of `update_application` enables a \
         locale, and `save_translations` writes the translations you produce — only the keys you name change, so \
         a correction is one key. Keep every `{{placeholder}}` name exactly; a \
         translation that changes one is refused.\n\
         \n\
         ## Whether you have the tools\n\
         \n\
         They arrive from an MCP server this project may or may not be connected \
         to: it is **off by default**, and reaching it needs a token an \
         administrator mints. If the tools named below are not in your tool list, \
         say so — the half of the work that needs them cannot be done from this \
         repository, and writing code against a schema you could not change is \
         the failure this file exists to prevent.\n\
         \n\
         ## The tools\n\
         \n\
         Every tool is refused for a caller that is not an administrator, and a \
         tool whose grant the token was not given is **still listed**: it refuses \
         the call naming the grant that would allow it, so an agent can report \
         what the administrator would have to tick rather than guess. The \
         grants are `allow_create`, `allow_edit`, `allow_drop` and \
         `allow_access_changes`; the composite tools check theirs per operation.\n\
         \n\
         ### Always offered\n\
         \n\
         {always}\
         \n\
         ### Offered when the token carries `{triggers_area}`\n\
         \n\
         {triggers}\
         \n\
         ### Offered when the token carries `{applications_area}`\n\
         \n\
         {applications}\
         \n\
         An area the token does not carry is not refused, it is **absent**: those \
         tools are not in `tools/list` at all.\n\
         \n\
         ## What this server deliberately does not offer\n\
         \n\
         - **Writing files.** There is no tool that edits this project's source. \
         You have the repository and an editor; a second way to write these files \
         would be a second thing to keep in step with them.\n\
         - **Row data.** No listing, inserting or updating of rows. Administering \
         a schema and reading the data in it are different capabilities, and only \
         the first is what building an application needs.\n\
         - **Users, backup and restore, and anything holding a provider's key.** \
         Each is an administrative act; none is one that building an application \
         requires.\n\
         \n\
         ## The order that works\n\
         \n\
         1. `describe_schema` before changing anything that already exists.\n\
         2. `edit_schema` with the **whole** change as one ordered batch — a \
         foreign key may point at a table created earlier in the same list. It is \
         one transaction: a refused operation refuses the batch, naming its index.\n\
         3. A table this application should serve must be **connected** to it: \
         `update_application` with `tables: {{ add: [...] }}`. An application reaches only its \
         connected tables, and the generated client is typed for them alone.\n\
         4. Read the result. It names the applications it re-projected and which \
         of them **want a build** — a schema change rewrites the generated client \
         but runs no bundler, so an application with a build is serving one made \
         against the old schema until you rebuild it.\n\
         5. `buildApplication` with the id from that result. It regenerates \
         `{client_file}` from the new schema, runs the build, and serves it with \
         no restart. A build that does not compile comes back as a result with \
         the diagnostics in it, not as a refusal.\n\
         6. Only then is the generated client in this repository in step with the \
         database, and only then does code written against it compile.\n",
        skill = skill_name(app),
        name = app.name,
        always = render(&always),
        triggers = render(&triggers),
        applications = render(&applications),
        triggers_area = Area::Triggers.key(),
        applications_area = Area::Applications.key(),
    )
}
