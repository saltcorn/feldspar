//! The opinionated `react` framework: conventions instead of settings (design
//! §13.3, TODO §2.1/§2.2).
//!
//! [`CodeFramework`](crate::CodeFramework) is the right shape for "any bundler,
//! any layout" and the wrong shape for the common case — it asks for five
//! mutually-consistent settings and then assumes a project the admin created over
//! SSH. `react` inverts that: it asks for **the file store and the project
//! directory**, and derives everything else.
//!
//! This module owns the derivations and nothing else. It is deliberately free of
//! I/O: the conventions are pure functions of the project name, so they can be
//! asserted directly, and the scaffolding that writes a project out (§2.3) reads
//! its paths from here rather than restating them. Serving is not reimplemented
//! either — a built React app is a static bundle with an SPA fallback, which
//! [`CodeFramework`](crate::CodeFramework) already serves; the difference between
//! the two frameworks is configuration and scaffolding, not serving.
//!
//! The conventions, for a project named `todo`:
//!
//! | `code` setting | `react` convention |
//! |---|---|
//! | `source`  | `todo` |
//! | `output`  | `todo/dist` |
//! | `command` | `npm run build` |
//! | `client`  | `todo/src/feldspar/client.ts` |

use sc_error::{Error, Result};
use sc_types::{BasicType, FormField};

use crate::application::CspPolicy;
use crate::framework::{BuildSpec, InstallSpec};

/// The registered name of the opinionated React framework.
pub const REACT_FRAMEWORK: &str = "react";

/// The `project` setting: the sub-directory of the store holding the app, and the
/// name every other path is derived from.
///
/// Deliberately *not* taken from [`Application::name`](crate::Application::name):
/// that is a human-facing display name (`"My Blog"`, renameable at will) while
/// this names a directory on disk that a build and a scaffold both point at. It
/// also has to live in the framework config because that is all
/// `app_source_from_config` is given.
///
/// **Blank means the store root.** A store dedicated to one application — the
/// common case for a git store cloned from the app's own repository — has no
/// sub-directory to name, and asking for one would force an admin to invent a
/// nesting level their repository does not have. So the setting is optional, and
/// every derived path ([`project_path`]) collapses to the store root when it is
/// empty.
pub const CFG_PROJECT: &str = "project";

/// The directory the bundler emits into, under the project directory.
pub const REACT_OUTPUT_SUBDIR: &str = "dist";

/// Where the generated client and hooks live inside the project — generated
/// output, overwritten on every build (TODO §2.1).
pub const REACT_RUNTIME_SUBDIR: &str = "src/feldspar";

/// The generated TypeScript client's file name within [`REACT_RUNTIME_SUBDIR`].
pub const REACT_CLIENT_FILE: &str = "client.ts";

/// The build command every scaffolded app is built with: Vite behind an npm
/// script, so the project's own `package.json` stays the place a developer
/// changes what building means.
pub const REACT_BUILD_COMMAND: &str = "npm";
/// The arguments to [`REACT_BUILD_COMMAND`].
pub const REACT_BUILD_ARGS: [&str; 2] = ["run", "build"];
/// The arguments that install a scaffolded project's dependencies.
pub const REACT_INSTALL_ARGS: [&str; 1] = ["install"];
/// The directory whose presence means the dependencies are already installed.
pub const REACT_INSTALL_MARKER: &str = "node_modules";

/// The settings the `react` framework needs: the file store, and the project
/// directory within it.
///
/// The whole point of the framework is what is *absent* here — `source`,
/// `output`, `command` and `client` are all derived from the project name (see
/// [`react_build_spec`] and [`react_client_path`]), so there is no way to
/// configure them into disagreeing with each other or with the scaffold.
///
/// A free function as well as a `config_spec` impl, for the same reason
/// [`code_config_spec`](crate::code_config_spec) is one: the admin UI and the
/// save-time check need the settings before any instance exists.
pub fn react_config_spec() -> Vec<FormField> {
    vec![
        // Same server-resolved pick-list as the `code` framework's store (§1.6):
        // the set of stores is runtime state, so the spec names a query rather
        // than listing them.
        FormField::new(crate::framework::CFG_STORE, BasicType::Text)
            .label("File store")
            .required()
            .server_query(sc_catalog::QUERY_FILE_STORES),
        // Optional, and defaulted to the store root, exactly like the `code`
        // framework's source directory: a store holding one application needs no
        // sub-directory, and a required field would make the admin invent one.
        FormField::new(CFG_PROJECT, BasicType::Text)
            .label("Project directory (blank for the store root)")
            .default_value(""),
    ]
}

/// A path under the project directory, relative to the file store.
///
/// The one place the "blank means the store root" rule is applied, so every
/// derived path — build output, generated runtime, the client — agrees on it and
/// none of them can produce a leading `/` that a store would have to forgive.
pub fn project_path(project: &str, rest: &str) -> String {
    if project.is_empty() {
        rest.to_owned()
    } else {
        format!("{project}/{rest}")
    }
}

/// How to name the project directory in a message to an admin: the directory
/// itself, or "the store root" when it is blank.
///
/// An error reading `cannot scaffold into `` of file store `apps`` names nothing;
/// the admin did not type an empty string, they left a box empty.
pub fn project_description(project: &str) -> String {
    if project.is_empty() {
        "the store root".to_owned()
    } else {
        format!("`{project}`")
    }
}

/// The build step for a project named `project`: `npm run build`, from
/// `<project>` into `<project>/dist`.
///
/// The same [`BuildSpec`] type the `code` framework produces, which is what keeps
/// the build and serve paths shared rather than forked — `build_app` cannot tell
/// which framework it is building.
pub fn react_build_spec(project: &str) -> BuildSpec {
    BuildSpec {
        command: REACT_BUILD_COMMAND.to_owned(),
        args: REACT_BUILD_ARGS.iter().map(|a| (*a).to_owned()).collect(),
        source_dir: project.to_owned(),
        output_dir: project_path(project, REACT_OUTPUT_SUBDIR),
        // The tutorial used to tell the admin to run this over SSH, which an
        // admin with no shell cannot. The framework knows its projects are npm
        // projects, so the build installs them when they are not installed.
        install: Some(InstallSpec {
            command: REACT_BUILD_COMMAND.to_owned(),
            args: REACT_INSTALL_ARGS.iter().map(|a| (*a).to_owned()).collect(),
            marker: REACT_INSTALL_MARKER.to_owned(),
        }),
    }
}

/// Where the app's generated runtime (client and typed hooks) is written,
/// relative to the file store: `<project>/src/feldspar`.
pub fn react_runtime_dir(project: &str) -> String {
    project_path(project, REACT_RUNTIME_SUBDIR)
}

/// Where the generated TypeScript client is written, relative to the file store:
/// `<project>/src/feldspar/client.ts`.
///
/// The scaffold imports from this path, so it is a convention shared by the
/// generator and the generated code rather than a setting either could get wrong.
pub fn react_client_path(project: &str) -> String {
    format!("{}/{REACT_CLIENT_FILE}", react_runtime_dir(project))
}

/// The default Content-Security-Policy for a scaffolded React app.
///
/// The admin is no longer choosing the build tooling, so they should not have to
/// work out the policy that tooling needs either. This is `default-src 'self'`
/// (the strict baseline) with exactly the widenings a Vite bundle actually
/// requires, and **no `'unsafe-inline'` or `'unsafe-eval'` anywhere** — which is
/// not a coincidence but a consequence of §2.1: Vite emits real module scripts
/// and a stylesheet file rather than inline ones, and the styling decision ruled
/// out CSS-in-JS precisely because runtime `<style>` injection would have forced
/// `style-src 'unsafe-inline'` into every app's policy.
///
/// - `img-src`/`font-src` allow `data:` because Vite inlines small assets as data
///   URIs (its `assetsInlineLimit`), so a strict policy would break an app for a
///   reason the admin cannot see in their own source.
/// - `img-src` and `media-src` allow `blob:` (TODO F1.7): an app previewing a
///   file before it uploads it, or showing one it fetched through its API as a
///   `Blob`, does so through `URL.createObjectURL`. A `blob:` URL names bytes
///   this page already holds, so it reaches no other origin.
/// - `connect-src 'self'` is where the app's API calls go; the app's only path to
///   data is its own origin's API providers (§13.4).
/// - `object-src 'none'`, `base-uri 'self'` and `frame-ancestors 'none'` are
///   tightenings, not widenings: no plugins, no `<base>` rewriting the app's own
///   URLs, and no framing the app for clickjacking.
///
/// The admin can still edit the policy on the application — this is a default,
/// not a fixture.
pub fn react_csp() -> CspPolicy {
    CspPolicy::strict()
        .directive("img-src", ["'self'", "data:", "blob:"])
        .directive("media-src", ["'self'", "blob:"])
        .directive("font-src", ["'self'", "data:"])
        .directive("connect-src", ["'self'"])
        .directive("object-src", ["'none'"])
        .directive("base-uri", ["'self'"])
        .directive("frame-ancestors", ["'none'"])
}

/// The naming rule itself.
///
/// This is stricter than "a path the file store will accept" on purpose. The name
/// is not only a directory: it is interpolated into a build path, a client path
/// and (in §2.3) a `package.json` `name`, so the safe set is the one every
/// consumer agrees on — ASCII letters, digits, `-` and `_`, starting with a letter
/// or digit. That rules out `..`, `/`, spaces and leading dots by construction
/// rather than by a traversal check further down, which is the difference between
/// a name that cannot escape the store and one that is checked not to.
/// [`valid_project_name`] as a check, with the error an admin should read.
///
/// **Blank is accepted**: it is not a bad directory name but the absence of one,
/// meaning the app lives at the store root (see [`CFG_PROJECT`]). The rule below
/// is about names that would be interpolated into a path, and there is nothing to
/// interpolate.
///
/// Used from `validate_framework_config`, so an unusable name is refused **on
/// save**, where the admin is still looking at the form — the same principle
/// §1.6 applied to an unknown store. A spec cannot express "a directory name"
/// (§6.2 has no pattern constraint), so this is the framework's own check rather
/// than something `validate_attrs` could have done.
pub fn check_project_name(project: &str) -> Result<()> {
    if project.trim().is_empty() || valid_project_name(project.trim()) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "setting `{CFG_PROJECT}` is {project:?}, which is not a usable directory name; \
         use letters, digits, `-` and `_`, starting with a letter or digit, \
         or leave it blank to put the project at the root of the file store"
    )))
}

/// Whether `project` is usable as the project directory name.
///
/// A *name*, so the empty string is not one — [`check_project_name`] is where
/// "no directory at all" is allowed, because that is a statement about the
/// setting rather than about the name.
pub fn valid_project_name(project: &str) -> bool {
    let mut chars = project.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::CFG_STORE;

    #[test]
    fn the_spec_is_two_labelled_settings() {
        let spec = react_config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        // The whole claim of the framework: five settings become two.
        assert_eq!(names, [CFG_STORE, CFG_PROJECT]);
        assert!(spec.iter().all(|f| !f.base.label.is_empty()));

        // The store is the same server-resolved pick-list `code` uses, so the
        // admin UI renders a select with no framework-specific code (§1.6).
        let store = spec.iter().find(|f| f.name() == CFG_STORE).unwrap();
        assert!(store.required);
        assert_eq!(store.query(), Some(sc_catalog::QUERY_FILE_STORES));
        // The project is free text — it is a name the admin invents.
        let project = spec.iter().find(|f| f.name() == CFG_PROJECT).unwrap();
        assert_eq!(project.query(), None);
        assert!(project.static_options().is_empty());
        // ...and it is optional, defaulted to the store root: a store holding one
        // app has no sub-directory to name, and a required field would leave the
        // admin unable to save the form at all.
        assert!(!project.required);
        assert_eq!(project.default.as_ref().and_then(|d| d.as_str()), Some(""));
    }

    #[test]
    fn every_path_is_derived_from_the_project_name() {
        let build = react_build_spec("todo");
        assert_eq!(build.command, "npm");
        assert_eq!(build.args, ["run", "build"]);
        assert_eq!(build.source_dir, "todo");
        assert_eq!(build.output_dir, "todo/dist");
        assert_eq!(react_runtime_dir("todo"), "todo/src/feldspar");
        assert_eq!(react_client_path("todo"), "todo/src/feldspar/client.ts");

        // The client lives under the runtime directory, not beside it: §2.1's
        // "generated, overwritten" boundary is one directory, so the scaffold has
        // one thing to declare generated.
        assert!(react_client_path("blog").starts_with(&react_runtime_dir("blog")));
        // And everything stays inside the project directory, so two apps in one
        // store cannot build over each other.
        for path in [
            build.source_dir,
            build.output_dir,
            react_runtime_dir("todo"),
            react_client_path("todo"),
        ] {
            assert!(path == "todo" || path.starts_with("todo/"), "{path}");
        }
    }

    #[test]
    fn a_blank_project_puts_everything_at_the_store_root() {
        let build = react_build_spec("");
        // The source directory *is* the store, and nothing acquires a leading
        // slash the store would have to forgive.
        assert_eq!(build.source_dir, "");
        assert_eq!(build.output_dir, "dist");
        assert_eq!(react_runtime_dir(""), "src/feldspar");
        assert_eq!(react_client_path(""), "src/feldspar/client.ts");
        assert_eq!(project_path("", "package.json"), "package.json");
        assert_eq!(project_path("todo", "package.json"), "todo/package.json");
        for path in [
            build.output_dir,
            react_runtime_dir(""),
            react_client_path(""),
            project_path("", "src/App.tsx"),
        ] {
            assert!(!path.starts_with('/'), "{path}");
        }

        // Blank is the absence of a directory, not a bad one, so it saves...
        assert!(check_project_name("").is_ok());
        assert!(check_project_name("   ").is_ok());
        // ...while a name that would still be interpolated into a path does not.
        assert!(check_project_name("../..").is_err());
        // And an admin reading a message about it is told where it is.
        assert_eq!(project_description(""), "the store root");
        assert_eq!(project_description("todo"), "`todo`");
    }

    #[test]
    fn a_project_name_is_a_plain_identifier() {
        for ok in ["todo", "blog2", "my-app", "my_app", "a"] {
            assert!(valid_project_name(ok), "{ok} should be accepted");
        }
        // Traversal, separators and shell-awkward names are excluded by the
        // charset rather than caught by a later check.
        for bad in [
            "", "..", ".", ".hidden", "a/b", "a\\b", "my app", "-lead", "app!", "app.js", "app;rm",
            "Ünicode",
        ] {
            assert!(!valid_project_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn the_default_csp_needs_no_unsafe_source() {
        let header = react_csp().header_value();
        // The property that makes the tooling decision defensible (§2.1): a Vite
        // bundle needs no inline-script or inline-style escape hatch.
        assert!(!header.contains("unsafe-inline"), "{header}");
        assert!(!header.contains("unsafe-eval"), "{header}");
        // Still anchored on the strict baseline...
        assert!(header.contains("default-src 'self'"), "{header}");
        // ...widened only where Vite's own output requires it...
        assert!(header.contains("img-src 'self' data: blob:;"), "{header}");
        // ...and for object URLs of bytes the page already holds (F1.7)...
        assert!(header.contains("media-src 'self' blob:;"), "{header}");
        assert!(header.contains("font-src 'self' data:;"), "{header}");
        // ...and the app's data path is its own origin's API.
        assert!(header.contains("connect-src 'self'"), "{header}");
        assert!(header.contains("frame-ancestors 'none'"), "{header}");
    }
}
