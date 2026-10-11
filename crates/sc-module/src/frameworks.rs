//! A module's **application frameworks**, as `sc-app`'s [`FrameworkHost`]
//! (design §13.3, §15.1).
//!
//! A v1 plugin has no such key — this is *this* system's, like `modelproviders`
//! — and it is what makes "an admin picks a framework" reach past the two
//! frameworks written in Rust. A module exports:
//!
//! ```js
//! frameworks: {
//!   vue: {
//!     label: "Vue",
//!     config_fields: [{ name: "store", type: "String", required: true }],
//!     build: { store: "{{ store }}", source: "{{ project }}",
//!              output: "{{ project }}/dist", command: "npm run build",
//!              runtime: "{{ project }}/src/feldspar", client: "client.ts" },
//!     scaffold: async (ctx) => [{ path: "package.json", contents: "…" }],
//!     runtime: async (ctx) => [{ path: `${ctx.runtime}/composables.ts`, contents: "…" }],
//!   },
//! }
//! ```
//!
//! and an application whose `_fd_applications` row says `framework = "vue"` is
//! served, built, scaffolded and configured through it — by the same code that
//! serves, builds, scaffolds and configures a `react` app, because what a
//! framework *is* to `sc-app` is a settings spec, a build spec, a CSP and a file
//! generator, and all four are things this translates.
//!
//! # Two halves, and why they are different shapes
//!
//! The **declaration** is read once, when the module loads, and installed as
//! values ([`FrameworkDecl`]). It has to be: `framework_config_spec` renders a
//! form, `app_source_from_config` resolves a build, and both are synchronous
//! calls on paths that have no worker and no `await` to spare. Nothing in the
//! declaration depends on run-time state, so nothing is lost by reading it early.
//!
//! The **generator** stays in the worker and is reached through
//! [`framework_files`](FrameworkHost::framework_files), because what it produces
//! depends on the application — its tables, its API surface, its roles — which
//! the plugin author did not know.
//!
//! # A broken declaration costs the framework, not the module
//!
//! A template that will not parse, a build with no command, a name a built-in
//! already has: each drops **that framework** and is reported on the module's
//! card ([`issues`](ModuleFrameworks::issues)). The module still loads and its
//! actions still run — the rule every other facility key here follows, for the
//! reason [`crate::spec`] gives: a module with one odd declaration is a module
//! with one odd declaration.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use sc_app::{
    BuildTemplate, CspPolicy, DeclaredFile, FilePhase, FrameworkDecl, FrameworkHost, InstallSpec,
    OperationAnswer, TargetOperation, TargetRequirement, TargetRequirementKind, TargetTemplate,
};
use sc_error::{Error, Result};
use sc_expr::Template;
use serde_json::Value as Json;

use crate::host::{FrameworkManifest, ModuleHost};
use crate::modules::ModuleSet;
use crate::spec::{config_fields_to_form_fields, show_if_conditions};

/// The frameworks this server's JavaScript modules supply, over the pool they
/// run on.
pub struct ModuleFrameworks {
    host: Arc<ModuleHost>,
    frameworks: Vec<(String, FrameworkDecl)>,
    issues: Vec<String>,
}

impl ModuleFrameworks {
    /// Every framework of every loaded module in `set`, over `host`.
    ///
    /// A module that would not load supplies none: it has no manifest, and a
    /// framework nobody can build with is not one to offer in the application
    /// form.
    ///
    /// **First declaration of a name wins**, and the loser is reported naming
    /// both modules. Framework names share one namespace because an application
    /// stores one — the same rule, and the same reason, as two modules claiming
    /// one action name.
    pub fn new(host: &Arc<ModuleHost>, set: &ModuleSet) -> ModuleFrameworks {
        let mut frameworks: Vec<(String, FrameworkDecl)> = Vec::new();
        let mut issues = Vec::new();
        for loaded in set.modules() {
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            let module = loaded.module.name.clone();
            for framework in &manifest.frameworks {
                if let Some((other, _)) = frameworks.iter().find(|(_, f)| f.name == framework.name)
                {
                    issues.push(format!(
                        "the framework `{}` declared by {module} is not available: {other} \
                         already declares a framework of that name, and an application \
                         stores a framework by name",
                        framework.name
                    ));
                    continue;
                }
                match declaration(framework, &module) {
                    Ok(decl) => frameworks.push((module.clone(), decl)),
                    Err(e) => issues.push(format!(
                        "the framework `{}` declared by {module} is not available: {e}",
                        framework.name
                    )),
                }
            }
        }
        ModuleFrameworks {
            host: Arc::clone(host),
            frameworks,
            issues,
        }
    }

    /// An empty set — a server with no modules, and the starting point for a
    /// test that wants the seam without a worker.
    pub fn empty(host: &Arc<ModuleHost>) -> ModuleFrameworks {
        ModuleFrameworks {
            host: Arc::clone(host),
            frameworks: Vec::new(),
            issues: Vec::new(),
        }
    }

    /// What could not be translated, one sentence each.
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    /// Which module declared `name`, refusing a name this set does not have
    /// before a worker is reached.
    ///
    /// Re-checked here even though `sc-app` resolved the framework through this
    /// same list: a module can be deleted between an application form being
    /// rendered and its scaffold button being pressed, and the honest answer then
    /// is a sentence naming what went away rather than a worker's "not loaded in
    /// this host".
    fn require(&self, name: &str) -> Result<&str> {
        self.frameworks
            .iter()
            .find(|(_, f)| f.name == name)
            .map(|(module, _)| module.as_str())
            .ok_or_else(|| {
                Error::not_found(format!(
                    "no installed module supplies the framework `{name}`; it may have been \
                     uninstalled, or failed to load"
                ))
            })
    }
}

#[async_trait]
impl FrameworkHost for ModuleFrameworks {
    fn frameworks(&self) -> Vec<FrameworkDecl> {
        self.frameworks.iter().map(|(_, f)| f.clone()).collect()
    }

    async fn framework_files(
        &self,
        name: &str,
        phase: FilePhase,
        context: Json,
    ) -> Result<Vec<DeclaredFile>> {
        let module = self.require(name)?.to_owned();
        let answer = self
            .host
            .framework_files(&module, name, phase.as_str(), &context)
            .await?;
        let Json::Array(files) = answer else {
            return Err(Error::config(format!(
                "the {} of framework `{name}` of module {module} answered {answer}, which \
                 is not a list of files",
                phase.as_str()
            )));
        };
        files
            .iter()
            .map(|file| declared_file(file, name, &module, phase))
            .collect()
    }

    async fn call_target_operation(
        &self,
        name: &str,
        target: &str,
        operation: &str,
        context: Json,
    ) -> Result<OperationAnswer> {
        let module = self.require(name)?.to_owned();
        let answer = self
            .host
            .call_target_operation(&module, name, target, operation, &context)
            .await?;
        operation_answer(&answer, operation, &module)
    }
}

/// An operation's answer, `{ files: [{ path, base64 }], settings, message }`,
/// checked rather than trusted, as a generated file is ([`declared_file`]): it
/// is whatever the module's JavaScript returned.
fn operation_answer(answer: &Json, operation: &str, module: &str) -> Result<OperationAnswer> {
    use base64::Engine as _;
    let bad = |what: String| {
        Error::config(format!(
            "operation `{operation}` of module {module} answered {what}"
        ))
    };
    let mut out = OperationAnswer {
        message: answer
            .get("message")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned(),
        ..OperationAnswer::default()
    };
    match answer.get("settings") {
        None | Some(Json::Null) => {}
        Some(Json::Object(settings)) => out.settings = settings.clone(),
        Some(other) => return Err(bad(format!("settings that are not an object: {other}"))),
    }
    let files = match answer.get("files") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(files)) => files.clone(),
        Some(other) => return Err(bad(format!("files that are not a list: {other}"))),
    };
    for file in &files {
        let path = file
            .get("path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| bad("a file with no path".to_owned()))?;
        let bytes = file
            .get("base64")
            .and_then(Json::as_str)
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .ok_or_else(|| bad(format!("the file `{path}` without base64 contents")))?;
        out.files.push((path.to_owned(), bytes));
    }
    Ok(out)
}

/// One `{ path, contents }` answer.
///
/// Checked here as well as in the host script, because the two checks answer to
/// different owners: the script's protects the module author from their own
/// generator, and this one protects the file store from a host that is not this
/// server's script — a module worker is a process, and what comes back over a
/// pipe is data until it has been read.
fn declared_file(
    file: &Json,
    framework: &str,
    module: &str,
    phase: FilePhase,
) -> Result<DeclaredFile> {
    let bad = |what: &str| {
        Error::config(format!(
            "the {} of framework `{framework}` of module {module} answered a file {what}",
            phase.as_str()
        ))
    };
    let path = file
        .get("path")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| bad("with no path"))?;
    let contents = file
        .get("contents")
        .and_then(Json::as_str)
        .ok_or_else(|| bad(&format!("at `{path}` whose contents are not text")))?;
    Ok(DeclaredFile {
        path: path.to_owned(),
        contents: contents.to_owned(),
    })
}

/// Translate one manifest entry into the declaration `sc-app` installs.
fn declaration(manifest: &FrameworkManifest, module: &str) -> Result<FrameworkDecl> {
    let name = manifest.name.trim();
    if name.is_empty() {
        return Err(Error::invalid(
            "it has no name, and an application stores a framework by name".to_owned(),
        ));
    }
    // The issues from translating the fields are dropped here rather than
    // collected, for the reason the table providers drop theirs: they are already
    // the module's, the loader records them on its card, and reporting them again
    // on every application form would put a module's problem in front of an admin
    // creating an unrelated app.
    let (mut config_spec, _) =
        config_fields_to_form_fields(&manifest.config_fields, &format!("the framework `{name}`"));
    let options = target_options(&manifest.targets, &mut config_spec, name)?;
    Ok(FrameworkDecl {
        name: name.to_owned(),
        module: module.to_owned(),
        label: manifest.label.trim().to_owned(),
        description: manifest.description.trim().to_owned(),
        targets: targets(&manifest.targets, &config_spec, &options)?,
        build: build_template(&manifest.build, &config_spec)?,
        config_spec,
        csp: csp(&manifest.csp),
        builder_prompt: match manifest.builder_prompt.trim() {
            "" => None,
            prompt => Some(Template::parse(prompt)?),
        },
        checks: checks(&manifest.checks)?,
        scaffolds: manifest.scaffolds,
    })
}

/// The declared checks: script names, each once. Refused when the module loads,
/// for the reason a bad template is — the agent's save would refuse them later,
/// on an application form that is not the module author's.
fn checks(declared: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in declared.iter().map(|n| n.trim()) {
        if name.is_empty() {
            return Err(Error::invalid(
                "its `checks` lists an empty script name".to_owned(),
            ));
        }
        if out.iter().any(|n| n == name) {
            return Err(Error::invalid(format!("its `checks` lists `{name}` twice")));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

/// The `targets` object — the extra things a framework can build besides the web
/// app, such as an Android APK:
///
/// ```js
/// targets: {
///   android: { label, command, artifact, env: { … }, requires: [ … ] },
/// }
/// ```
///
/// This is the load-time half, the sibling of [`build_template`]: it reads and
/// checks the declaration once, when the module loads. The per-application half
/// is `sc_app::FrameworkDecl::target_spec`, which fills the templates in with
/// one app's settings.
///
/// Refused whole when one target is malformed, for the reason a bad `build` is:
/// the author hears about it on the module's card when it loads, not the admin
/// when they press the button.
fn targets(
    declared: &Json,
    spec: &[sc_types::FormField],
    options: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<TargetTemplate>> {
    let Json::Object(entries) = declared else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, target) in entries {
        let text = |key: &str| {
            target
                .get(key)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let missing =
            |key: &str| Error::invalid(format!("its target `{name}` declares no `{key}`"));
        let command = text("command").ok_or_else(|| missing("command"))?;
        let artifact_source = text("artifact").ok_or_else(|| missing("artifact"))?;
        out.push(TargetTemplate {
            name: name.clone(),
            label: text("label").unwrap_or(name).to_owned(),
            command: settings_template(command, spec, &format!("target `{name}`'s `command`"))?,
            artifact: settings_template(
                artifact_source,
                spec,
                &format!("target `{name}`'s `artifact`"),
            )?,
            env: target_env(name, target.get("env"), spec)?,
            requires: target_requires(name, target.get("requires"))?,
            options: options.get(name).cloned().unwrap_or_default(),
            operations: parse_target_operations(name, target.get("operations"))?,
        });
    }
    Ok(out)
}

/// Read a target's buttons — its `operations` — from the module's declaration.
///
/// In the plugin, an operation looks like this:
///
/// ```js
/// operations: {
///   generate_keystore: {
///     label: "Generate a keystore",           // the button's text (required)
///     description: "Creates a new key …",     // shown under the button
///     showIf: { own_keystore: true },         // when the button is shown
///     run: generateKeystore,                  // what pressing it does
///   },
/// }
/// ```
///
/// Everything except `run` is read here, once, when the module loads, so the
/// form can show the button without asking the module. `run` is a function, so
/// it stays inside the module, where it is called when the button is pressed.
///
/// No `operations` means no buttons. A malformed one — not an object, or a
/// button without a label — is refused when the module loads, so its author
/// sees the mistake on the module's card rather than an admin seeing a broken
/// button.
fn parse_target_operations(target: &str, declared: Option<&Json>) -> Result<Vec<TargetOperation>> {
    let entries = match declared {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Object(by_name)) => by_name,
        Some(_) => {
            return Err(Error::invalid(format!(
                "its target `{target}`'s `operations` is not an object of operations by name"
            )));
        }
    };
    let mut out = Vec::new();
    for (name, operation) in entries {
        let text = |key: &str| {
            operation
                .get(key)
                .and_then(Json::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_owned()
        };
        let label = text("label");
        if label.is_empty() {
            return Err(Error::invalid(format!(
                "its target `{target}`'s operation `{name}` has no `label` for its button"
            )));
        }
        out.push(TargetOperation {
            name: name.clone(),
            label,
            description: text("description"),
            show_if: show_if_conditions(operation.get("showIf")),
        });
    }
    Ok(out)
}

/// Each target's `options`: settings that configure that target alone, in the
/// same field vocabulary as the framework's `config_fields`.
///
/// ```js
/// targets: { android: { options: [{ name: "app_version", type: "String" }], … } }
/// ```
///
/// They are **appended to the framework's settings** (`spec`), so an
/// application stores, validates and hands them to the framework's generators
/// exactly as it does its other settings, and the target's templates may
/// interpolate them. What is returned is which names belong to which target, so
/// a form can show them under the target. A name that is already a setting, or
/// another target's option, is refused: one application config holds them all.
fn target_options(
    declared: &Json,
    spec: &mut Vec<sc_types::FormField>,
    framework: &str,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    let Json::Object(entries) = declared else {
        return Ok(out);
    };
    for (target, declaration) in entries {
        let fields = match declaration.get("options") {
            None | Some(Json::Null) => continue,
            Some(Json::Array(fields)) => fields,
            Some(_) => {
                return Err(Error::invalid(format!(
                    "its target `{target}`'s `options` is not a list of fields"
                )));
            }
        };
        let (fields, _) = config_fields_to_form_fields(
            fields,
            &format!("the framework `{framework}`'s target `{target}`"),
        );
        let mut names = Vec::new();
        for field in fields {
            if spec.iter().any(|f| f.name() == field.name()) {
                return Err(Error::invalid(format!(
                    "its target `{target}`'s option `{}` has the name of a setting it already \
                     has; every setting and option shares one namespace",
                    field.name()
                )));
            }
            names.push(field.name().to_owned());
            spec.push(field);
        }
        out.insert(target.clone(), names);
    }
    Ok(out)
}

/// A target's `env`: `{ ANDROID_HOME: "/opt/sdk" }`, the variables its command is
/// started with.
///
/// Each value is a template over the framework's settings, so an application's
/// own setting (a keystore password) reaches the build here rather than through
/// a file in its project.
///
/// No `.env` file is written: the map is kept in memory, rebuilt whenever the
/// module (re)loads with its settings, and handed to the build process alone
/// when it starts (`Command::envs` in `sc_app`'s `run_target`).
///
/// A **blank value is left out** rather than set to nothing: a module fills
/// these from its own settings, and a setting the admin has not filled in should
/// leave the variable to the server's environment, not blank it for the build.
/// A name a process environment cannot carry — empty, or with `=` or a NUL — is
/// refused, as is a value that is not text.
fn target_env(
    target: &str,
    declared: Option<&Json>,
    spec: &[sc_types::FormField],
) -> Result<BTreeMap<String, Template>> {
    let mut env = BTreeMap::new();
    let Some(json) = declared.filter(|v| !v.is_null()) else {
        return Ok(env);
    };
    let Json::Object(vars) = json else {
        return Err(Error::invalid(format!(
            "its target `{target}`'s `env` is not an object of variable names and values"
        )));
    };
    for (key, value) in vars {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(Error::invalid(format!(
                "its target `{target}`'s `env` names {key:?}, which is not an environment \
                 variable a process can be started with"
            )));
        }
        let value = match value {
            Json::Null => continue,
            Json::String(text) => text.trim(),
            _ => {
                return Err(Error::invalid(format!(
                    "its target `{target}`'s `env` gives `{key}` a value that is not text"
                )));
            }
        };
        if value.contains('\0') {
            return Err(Error::invalid(format!(
                "its target `{target}`'s `env` gives `{key}` a value with a NUL in it"
            )));
        }
        if !value.is_empty() {
            env.insert(
                key.clone(),
                settings_template(value, spec, &format!("target `{target}`'s `env` `{key}`"))?,
            );
        }
    }
    Ok(env)
}

/// A target's `requires`: what the machine must have before it can build.
///
/// ```js
/// requires: [
///   { env: "ANDROID_HOME", directory: true, hint: "Set the Android SDK directory …" },
///   { command: "pod", hint: "Install CocoaPods." },
///   { os: "macos" },
/// ]
/// ```
///
/// Each entry names **exactly one** of `env`, `command` and `os`; `directory`
/// belongs with `env` alone, `dir_env` (a variable naming one more directory
/// to look for the program in) with `command` alone, and `hint` with any. Anything else is refused when
/// the module loads — a check that silently checks nothing would let a target
/// reach Gradle with the SDK it says it needs still missing.
fn target_requires(target: &str, declared: Option<&Json>) -> Result<Vec<TargetRequirement>> {
    let Some(json) = declared.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let Json::Array(entries) = json else {
        return Err(Error::invalid(format!(
            "its target `{target}`'s `requires` is not a list"
        )));
    };
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let bad = |why: &str| {
            Error::invalid(format!(
                "its target `{target}`'s `requires` entry {index} {why}"
            ))
        };
        let Json::Object(entry) = entry else {
            return Err(bad("is not an object"));
        };
        let text = |key: &str| -> Result<Option<String>> {
            match entry.get(key) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                Some(_) => Err(bad(&format!("gives `{key}` something that is not a name"))),
            }
        };
        let (env, command, os) = (text("env")?, text("command")?, text("os")?);
        let dir_env = text("dir_env")?;
        if dir_env.is_some() && command.is_none() {
            return Err(bad(
                "sets `dir_env`, which only a `command` requirement has",
            ));
        }
        let directory = match entry.get("directory") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => return Err(bad("gives `directory` a value that is not true or false")),
        };
        let kind = match (env, command, os) {
            (Some(name), None, None) => TargetRequirementKind::Env { name, directory },
            (None, Some(name), None) if !directory => {
                TargetRequirementKind::Command { name, dir_env }
            }
            (None, None, Some(name)) if !directory => TargetRequirementKind::Os { name },
            (None, None, None) => return Err(bad("names none of `env`, `command` and `os`")),
            _ if directory && entry.get("env").is_none() => {
                return Err(bad("sets `directory`, which only an `env` requirement has"));
            }
            _ => return Err(bad("names more than one of `env`, `command` and `os`")),
        };
        let hint = match entry.get("hint") {
            None | Some(Json::Null) => String::new(),
            Some(Json::String(s)) => s.trim().to_owned(),
            Some(_) => return Err(bad("gives `hint` something that is not text")),
        };
        out.push(TargetRequirement { kind, hint });
    }
    Ok(out)
}

/// The `build` object: five templates, a command and an optional install step.
///
/// Every template is checked **here** against the framework's own settings, so a
/// declaration naming a setting it does not have is refused when the module loads
/// — on the module's card, where its author's name is — rather than at the moment
/// an admin presses Build.
fn build_template(build: &Json, spec: &[sc_types::FormField]) -> Result<BuildTemplate> {
    let optional = |key: &str| -> Result<Option<Template>> {
        build
            .get(key)
            .and_then(Json::as_str)
            .map(|source| settings_template(source, spec, &format!("`{key}`")))
            .transpose()
    };
    // A path the framework must have: absent is a declaration that cannot resolve
    // to a directory, which is the same failure as one that will not parse.
    let required = |key: &str| -> Result<Template> {
        optional(key)?.ok_or_else(|| Error::invalid(format!("it declares no `{key}`")))
    };
    let command = build
        .get("command")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::invalid("it declares no build `command`".to_owned()))?;
    Ok(BuildTemplate {
        store: required("store")?,
        source: required("source")?,
        output: required("output")?,
        command: command.to_owned(),
        install: install(build.get("install")),
        runtime: optional("runtime")?,
        client_file: build
            .get("client")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or(sc_app::REACT_CLIENT_FILE)
            .to_owned(),
    })
}

/// Parse a template and check that every `{{ name }}` in it is one of the
/// framework's settings. `what` names the field in the error, as in "its
/// `store`" or "its target `android`'s `artifact`".
fn settings_template(source: &str, spec: &[sc_types::FormField], what: &str) -> Result<Template> {
    let template =
        Template::parse(source).map_err(|e| Error::invalid(format!("its {what}: {e}")))?;
    for setting in template.identifiers()? {
        if !spec.iter().any(|f| f.name() == setting) {
            return Err(Error::invalid(format!(
                "its {what} interpolates `{setting}`, which is not one of its settings ({})",
                settings_list(spec)
            )));
        }
    }
    Ok(template)
}

/// The settings a framework has, for an error message that must show the author
/// what they could have written.
fn settings_list(spec: &[sc_types::FormField]) -> String {
    if spec.is_empty() {
        return "it declares none".to_owned();
    }
    spec.iter()
        .map(|f| format!("`{}`", f.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `install: { command: "npm install", marker: "node_modules" }`.
///
/// Absent, or missing either half, means the framework does not manage the
/// project's dependencies — which is a legitimate declaration (`code`'s), and the
/// safe way to read a half-written one: a build that does not install is slower
/// to diagnose than one that does, but an install with no marker would run on
/// every single build.
fn install(declared: Option<&Json>) -> Option<InstallSpec> {
    let declared = declared?;
    let line = declared.get("command")?.as_str()?.trim();
    let marker = declared.get("marker")?.as_str()?.trim();
    if line.is_empty() || marker.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace().map(str::to_owned);
    Some(InstallSpec {
        command: parts.next()?,
        args: parts.collect(),
        marker: marker.to_owned(),
    })
}

/// `csp: { "img-src": ["'self'", "data:"] }` — the widenings, on top of the
/// strict baseline that `sc-app` applies.
///
/// A directive whose value is not a list of strings is dropped rather than
/// refused: the alternative is losing a whole framework over one malformed
/// directive, and what is lost instead is a widening — which fails *closed*, in
/// the direction a content-security policy should fail.
fn csp(declared: &Json) -> Option<CspPolicy> {
    let Json::Object(directives) = declared else {
        return None;
    };
    let mut policy = CspPolicy {
        directives: Default::default(),
    };
    for (directive, sources) in directives {
        let Json::Array(sources) = sources else {
            continue;
        };
        let sources: Vec<String> = sources
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_owned)
            .collect();
        if !sources.is_empty() {
            policy.directives.insert(directive.clone(), sources);
        }
    }
    (!policy.directives.is_empty()).then_some(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(value: Json) -> FrameworkManifest {
        serde_json::from_value(value).expect("the test's own manifest deserialises")
    }

    fn vue() -> Json {
        json!({
            "name": "vue",
            "label": "Vue",
            "description": "A Vue 3 project.",
            "config_fields": [
                { "name": "store", "type": "String", "required": true },
                { "name": "project", "type": "String", "default": "" }
            ],
            "build": {
                "store": "{{ store }}",
                "source": "{{ project }}",
                "output": "{{ project }}/dist",
                "command": "npm run build",
                "install": { "command": "npm install", "marker": "node_modules" },
                "runtime": "{{ project }}/src/feldspar",
                "client": "client.ts"
            },
            "csp": { "img-src": ["'self'", "data:"] },
            "builder_prompt": "You maintain {{ app }}.",
            "checks": ["typecheck"],
            "scaffolds": true
        })
    }

    #[test]
    fn declared_checks_are_script_names_each_once() {
        let mut twice = vue();
        twice["checks"] = json!(["typecheck", " typecheck "]);
        let err = declaration(&manifest(twice), "@feldspar/vue").unwrap_err();
        assert!(err.to_string().contains("`typecheck` twice"), "{err}");
        let mut blank = vue();
        blank["checks"] = json!([""]);
        assert!(declaration(&manifest(blank), "@feldspar/vue").is_err());
        // And a framework that names none has none.
        let mut none = vue();
        none.as_object_mut().unwrap().remove("checks");
        assert!(
            declaration(&manifest(none), "@feldspar/vue")
                .unwrap()
                .checks
                .is_empty()
        );
    }

    #[test]
    fn a_declaration_becomes_the_registry_entry_sc_app_installs() {
        let decl = declaration(&manifest(vue()), "@feldspar/vue").unwrap();
        assert_eq!(decl.name, "vue");
        assert_eq!(decl.module, "@feldspar/vue");
        assert_eq!(decl.label, "Vue");
        assert!(decl.scaffolds);
        assert_eq!(decl.checks, ["typecheck"]);
        // The settings arrive through the same translation an action's do.
        let names: Vec<&str> = decl.config_spec.iter().map(|f| f.name()).collect();
        assert_eq!(names, ["store", "project"]);
        assert!(decl.config_spec[0].required);
        // And the paths are the templates, resolvable against those settings.
        let config = [
            ("store".to_owned(), json!("apps")),
            ("project".to_owned(), json!("todo")),
        ]
        .into_iter()
        .collect();
        assert_eq!(decl.store(&config).unwrap(), "apps");
        let spec = decl.build_spec(&config).unwrap();
        assert_eq!(spec.command, "npm");
        assert_eq!(spec.output_dir, "todo/dist");
        assert_eq!(
            spec.install.unwrap().marker,
            "node_modules",
            "the install step is read whole or not at all"
        );
        assert_eq!(
            decl.client_path(&config).unwrap().unwrap(),
            "todo/src/feldspar/client.ts"
        );
        assert_eq!(
            decl.default_csp().directives["img-src"],
            ["'self'", "data:"]
        );
    }

    #[test]
    fn a_template_naming_a_setting_the_framework_has_not_got_is_refused_on_load() {
        // The point of checking here: the author hears about it on the module's
        // card, not the admin at the moment they press Build.
        let mut broken = vue();
        broken["build"]["output"] = json!("{{ projekt }}/dist");
        let msg = declaration(&manifest(broken), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("projekt"), "{msg}");
        assert!(
            msg.contains("`store`") && msg.contains("`project`"),
            "{msg}"
        );
    }

    #[test]
    fn a_build_that_cannot_say_how_to_build_is_refused() {
        let mut broken = vue();
        broken["build"] = json!({ "store": "{{ store }}", "source": "{{ project }}" });
        let msg = declaration(&manifest(broken), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("command"), "{msg}");

        let mut broken = vue();
        broken["build"]["output"] = Json::Null;
        let msg = declaration(&manifest(broken), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("output"), "{msg}");
    }

    #[test]
    fn a_targets_options_become_settings_its_templates_can_use() {
        let mut with_options = vue();
        with_options["targets"] = json!({
            "android": {
                "command": "npm run build:android:{{ build_type }}",
                "artifact": "{{ project }}/{{ build_type }}.apk",
                "options": [
                    { "name": "app_version", "label": "Version", "type": "String", "default": "1.0.0" },
                    { "name": "build_type", "type": "String", "default": "release",
                      "attributes": { "options": ["debug", "release"] } }
                ]
            }
        });
        let decl = declaration(&manifest(with_options), "@feldspar/vue").unwrap();
        // Appended to the framework's own settings, so they are stored and
        // validated like them…
        let names: Vec<&str> = decl.config_spec.iter().map(|f| f.name()).collect();
        assert_eq!(names, ["store", "project", "app_version", "build_type"]);
        // …and recorded as the target's, for the form.
        assert_eq!(decl.targets[0].options, ["app_version", "build_type"]);
        let config = [
            ("store".to_owned(), json!("apps")),
            ("project".to_owned(), json!("todo")),
            ("build_type".to_owned(), json!("debug")),
        ]
        .into_iter()
        .collect();
        let spec = decl.target_spec("android", &config).unwrap();
        assert_eq!(spec.args, ["run", "build:android:debug"]);
        assert_eq!(spec.artifact, "todo/debug.apk");
    }

    #[test]
    fn a_target_option_that_reuses_a_setting_name_or_is_not_a_list_is_refused() {
        let mut clash = vue();
        clash["targets"] = json!({
            "android": {
                "command": "npm run apk",
                "artifact": "x.apk",
                "options": [{ "name": "project", "type": "String" }]
            }
        });
        let msg = declaration(&manifest(clash), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("android") && msg.contains("project"), "{msg}");

        let mut not_a_list = vue();
        not_a_list["targets"] = json!({
            "android": { "command": "npm run apk", "artifact": "x.apk", "options": {} }
        });
        let msg = declaration(&manifest(not_a_list), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("options"), "{msg}");
    }

    #[test]
    fn a_target_command_naming_an_unknown_setting_is_refused_on_load() {
        let mut typo = vue();
        typo["targets"] = json!({
            "android": { "command": "npm run {{ varient }}", "artifact": "x.apk" }
        });
        let msg = declaration(&manifest(typo), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("varient") && msg.contains("command"), "{msg}");
    }

    #[test]
    fn declared_targets_become_templates_over_the_frameworks_settings() {
        let mut with_targets = vue();
        with_targets["targets"] = json!({
            "android": {
                "label": "Android APK",
                "command": "npm run build:android",
                "artifact": "{{ project }}/android/app-release.apk"
            }
        });
        let decl = declaration(&manifest(with_targets), "@feldspar/vue").unwrap();
        assert_eq!(decl.targets.len(), 1);
        let config = [
            ("store".to_owned(), json!("apps")),
            ("project".to_owned(), json!("todo")),
        ]
        .into_iter()
        .collect();
        let spec = decl.target_spec("android", &config).unwrap();
        assert_eq!(spec.label, "Android APK");
        assert_eq!(spec.args, ["run", "build:android"]);
        assert_eq!(spec.artifact, "todo/android/app-release.apk");

        // None declared is none, and costs nothing.
        assert!(
            declaration(&manifest(vue()), "@feldspar/vue")
                .unwrap()
                .targets
                .is_empty()
        );
    }

    #[test]
    fn a_targets_env_is_carried_and_a_blank_value_left_to_the_server() {
        let mut with_env = vue();
        with_env["targets"] = json!({
            "android": {
                "command": "npm run build:android",
                "artifact": "{{ project }}/a.apk",
                "env": { "ANDROID_HOME": " /opt/sdk ", "JAVA_HOME": "", "EXTRA": null }
            }
        });
        let decl = declaration(&manifest(with_env), "@feldspar/vue").unwrap();
        let config = [("store".to_owned(), json!("apps"))].into_iter().collect();
        let env = decl.target_spec("android", &config).unwrap().env;
        assert_eq!(
            env.get("ANDROID_HOME").map(String::as_str),
            Some("/opt/sdk")
        );
        // Unfilled settings do not blank the server's own variables.
        assert!(!env.contains_key("JAVA_HOME") && !env.contains_key("EXTRA"));

        let mut bad = vue();
        bad["targets"] = json!({
            "android": { "command": "x", "artifact": "a.apk", "env": { "A=B": "1" } }
        });
        let msg = declaration(&manifest(bad), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("android") && msg.contains("A=B"), "{msg}");
    }

    #[test]
    fn a_targets_env_carries_an_application_setting_and_drops_it_when_blank() {
        let mut with_secret = vue();
        with_secret["targets"] = json!({
            "android": {
                "command": "npm run apk",
                "artifact": "a.apk",
                "env": { "KEYSTORE_PASSWORD": "{{ keystore_password }}" },
                "options": [{ "name": "keystore_password", "type": "String", "input_type": "password" }]
            }
        });
        let decl = declaration(&manifest(with_secret), "@feldspar/vue").unwrap();
        let config = |password: &str| -> sc_types::Attrs {
            [
                ("store".to_owned(), json!("apps")),
                ("keystore_password".to_owned(), json!(password)),
            ]
            .into_iter()
            .collect()
        };
        let env = decl.target_spec("android", &config("s3cret")).unwrap().env;
        assert_eq!(env["KEYSTORE_PASSWORD"], "s3cret");
        let env = decl.target_spec("android", &config("")).unwrap().env;
        assert!(!env.contains_key("KEYSTORE_PASSWORD"));

        // A setting the framework has not got is refused on load, as in any
        // other template.
        let mut typo = vue();
        typo["targets"] = json!({
            "android": { "command": "x", "artifact": "a.apk", "env": { "P": "{{ pasword }}" } }
        });
        let msg = declaration(&manifest(typo), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("pasword"), "{msg}");
    }

    #[test]
    fn a_targets_operations_are_read_as_data_and_one_without_a_label_refused() {
        let mut with_ops = vue();
        with_ops["targets"] = json!({
            "android": {
                "command": "npm run apk",
                "artifact": "a.apk",
                "operations": {
                    "generate_keystore": {
                        "label": "Generate a keystore",
                        "description": "Makes one.",
                        "showIf": { "own_keystore": true }
                    }
                }
            }
        });
        let decl = declaration(&manifest(with_ops), "@feldspar/vue").unwrap();
        let op = &decl.targets[0].operations[0];
        assert_eq!(op.name, "generate_keystore");
        assert_eq!(op.label, "Generate a keystore");
        assert_eq!(op.description, "Makes one.");
        assert_eq!(
            op.show_if,
            [sc_types::ShowIfCondition::new(
                "own_keystore",
                vec![json!(true)]
            )]
        );

        let mut unlabelled = vue();
        unlabelled["targets"] = json!({
            "android": { "command": "x", "artifact": "a.apk", "operations": { "go": {} } }
        });
        let msg = declaration(&manifest(unlabelled), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("go") && msg.contains("label"), "{msg}");
    }

    #[test]
    fn an_operations_answer_is_read_and_a_malformed_one_refused() {
        let answer = operation_answer(
            &json!({
                "files": [{ "path": "keys/a.p12", "base64": "AAEC" }],
                "settings": { "keystore_alias": "upload" },
                "message": "done"
            }),
            "generate_keystore",
            "@feldspar/react-native",
        )
        .unwrap();
        assert_eq!(answer.files, [("keys/a.p12".to_owned(), vec![0u8, 1, 2])]);
        assert_eq!(answer.settings["keystore_alias"], json!("upload"));
        assert_eq!(answer.message, "done");

        for bad in [
            json!({ "files": [{ "path": "a", "base64": "!!" }] }),
            json!({ "files": [{ "base64": "AAEC" }] }),
            json!({ "files": "a" }),
            json!({ "settings": [1] }),
        ] {
            assert!(operation_answer(&bad, "op", "m").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_targets_requirements_are_read_and_a_malformed_one_is_refused() {
        let mut with_requires = vue();
        with_requires["targets"] = json!({
            "android": {
                "command": "npm run build:android",
                "artifact": "a.apk",
                "requires": [
                    { "env": "ANDROID_HOME", "directory": true, "hint": "Set the SDK." },
                    { "command": "pod", "dir_env": "POD_DIR" },
                    { "os": "macos" }
                ]
            }
        });
        let decl = declaration(&manifest(with_requires), "@feldspar/vue").unwrap();
        let requires = &decl.targets[0].requires;
        assert_eq!(
            requires[0].kind,
            TargetRequirementKind::Env {
                name: "ANDROID_HOME".to_owned(),
                directory: true
            }
        );
        assert_eq!(requires[0].hint, "Set the SDK.");
        assert_eq!(
            requires[1].kind,
            TargetRequirementKind::Command {
                name: "pod".to_owned(),
                dir_env: Some("POD_DIR".to_owned())
            }
        );
        assert_eq!(
            requires[2].kind,
            TargetRequirementKind::Os {
                name: "macos".to_owned()
            }
        );

        for (bad, says) in [
            (json!([{ "env": "A", "command": "b" }]), "more than one"),
            (json!([{ "hint": "x" }]), "none of"),
            (
                json!([{ "command": "pod", "directory": true }]),
                "`directory`",
            ),
            (json!([{ "env": "A", "dir_env": "B" }]), "`dir_env`"),
            (json!({ "env": "A" }), "not a list"),
        ] {
            let mut broken = vue();
            broken["targets"] = json!({
                "android": { "command": "x", "artifact": "a.apk", "requires": bad }
            });
            let msg = declaration(&manifest(broken), "@feldspar/vue")
                .unwrap_err()
                .to_string();
            assert!(
                msg.contains(says) && msg.contains("android"),
                "{says}: {msg}"
            );
        }
    }

    #[test]
    fn a_malformed_target_is_refused_on_load_naming_it() {
        let mut no_artifact = vue();
        no_artifact["targets"] = json!({ "android": { "command": "npm run apk" } });
        let msg = declaration(&manifest(no_artifact), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("android") && msg.contains("artifact"), "{msg}");

        let mut typo = vue();
        typo["targets"] = json!({
            "android": { "command": "npm run apk", "artifact": "{{ projekt }}/a.apk" }
        });
        let msg = declaration(&manifest(typo), "@feldspar/vue")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("projekt"), "{msg}");
    }

    #[test]
    fn a_framework_that_declares_no_runtime_gets_no_generated_client() {
        let mut plain = vue();
        plain["build"]["runtime"] = Json::Null;
        let decl = declaration(&manifest(plain), "@feldspar/vue").unwrap();
        let config = [("store".to_owned(), json!("apps"))].into_iter().collect();
        assert_eq!(decl.runtime_dir(&config).unwrap(), None);
        assert_eq!(decl.client_path(&config).unwrap(), None);
    }

    #[test]
    fn a_malformed_csp_directive_is_dropped_rather_than_widening_anything() {
        let mut odd = vue();
        odd["csp"] = json!({ "img-src": "not a list", "connect-src": ["'self'"] });
        let decl = declaration(&manifest(odd), "@feldspar/vue").unwrap();
        let policy = decl.default_csp();
        assert!(!policy.directives.contains_key("img-src"));
        assert_eq!(policy.directives["connect-src"], ["'self'"]);
        assert_eq!(policy.directives["default-src"], ["'self'"]);
    }
}
