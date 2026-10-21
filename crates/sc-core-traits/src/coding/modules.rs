//! `list_modules` and `install_module` — the Modules tab, for a coding agent,
//! behind `coding`'s `may_install_modules` grant.
//!
//! The reason is Python: a code body imports what is installed in the server's
//! Python environment and nothing else, so an agent writing a trigger that needs
//! `requests` or `pandas` has to be able to install it, or to stop and ask a
//! person to click Install. A JavaScript module is the same act through npm.
//!
//! **Installing a package runs somebody else's code with the server's
//! privileges** — npm's install scripts, a Python distribution's import, and for
//! Python no sandbox afterwards either. So the grant is off by default, implies no
//! other checkbox, and is offered only to a run whose caller is an **admin**,
//! exactly as the shell is ([`super::shell`]); the check is made when the tools
//! are listed and again when one is called.
//!
//! Both tools go through [`sc_agent::ModuleManager`], which the server answers
//! with the admin API's own `listModules` and `installModule`: an install here is
//! one row and one reload, listed and removable on the Modules tab like any
//! other. Nothing here touches the file store, so neither tool enters the change
//! ledger.

use sc_agent::{ModuleInstall, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use serde_json::{Value as Json, json};

use crate::files::{FileScope, string_arg};
use crate::table::arguments;

const ARG_LANGUAGE: &str = "language";
const ARG_SOURCE: &str = "source";
const ARG_LOCATION: &str = "location";

/// The listing tool one configured scope offers.
pub fn list_tool_name(scope: &FileScope) -> String {
    format!("list_modules_{}", scope.slug())
}

/// The install tool one configured scope offers.
pub fn install_tool_name(scope: &FileScope) -> String {
    format!("install_module_{}", scope.slug())
}

/// The listing tool's declaration.
pub fn list_spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        list_tool_name(scope),
        "List the modules installed on this server — npm packages and Python \
         distributions — with what each supplies and anything wrong with it; whether \
         npm, python and pip are available; and the bundled modules the server ships \
         with. A Python package listed here is importable from Python code bodies."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
}

/// The install tool's declaration.
pub fn install_spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        install_tool_name(scope),
        "Install a module on this server, or upgrade one by installing it again. For a \
         Python library that code bodies should be able to import, use language \
         `python`, source `pypi` and the PyPI specifier as location (e.g. `requests`, \
         `pandas>=2.2`, `httpx[http2]`); its dependencies are installed with it, and \
         it is importable from the next run on. Any distribution may be installed, not \
         only Saltcorn plugins. The package runs with the server's own privileges: \
         install only what the task needs, and say what you installed."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {
                ARG_LANGUAGE: {
                    "type": "string",
                    "enum": ["python", "javascript"],
                    "description": "The module's language.",
                },
                ARG_SOURCE: {
                    "type": "string",
                    "enum": ["pypi", "npm", "local", "bundled"],
                    "description": "Where it comes from: `pypi` or `npm` for a registry, \
                                    `local` for a directory on the server, `bundled` for \
                                    one the server ships (location is then its id).",
                },
                ARG_LOCATION: {
                    "type": "string",
                    "description": "The package specifier, directory or bundled id.",
                },
            },
            "required": [ARG_LANGUAGE, ARG_SOURCE, ARG_LOCATION],
            "additionalProperties": false,
        }),
    )
}

/// What is installed, as the Modules tab sees it.
pub async fn list(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    arguments(args, &[])?;
    ctx.require_modules()?.list().await
}

/// Install one package and answer with the loaded module.
pub async fn install(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let args = arguments(args, &[ARG_LANGUAGE, ARG_SOURCE, ARG_LOCATION])?;
    let request = ModuleInstall {
        language: string_arg(&args, ARG_LANGUAGE)?,
        source: string_arg(&args, ARG_SOURCE)?,
        location: string_arg(&args, ARG_LOCATION)?,
    };
    if request.location.trim().is_empty() {
        return Err(Error::invalid(
            "`location` names what to install and cannot be empty",
        ));
    }
    let module = ctx.require_modules()?.install(request).await?;
    Ok(json!({
        "installed": module,
        "note": "Installed and loaded. A Python package is importable from code bodies \
                 from their next run; a module that supplies actions, functions or \
                 providers offers them now.",
    }))
}
