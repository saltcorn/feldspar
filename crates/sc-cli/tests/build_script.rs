//! The build script's one decision: does `cargo build` also build the admin UI
//! and the IDE (`ui/admin`, `ui/ide`)?
//!
//! It is on by default and turned off by `SC_BUILD_ADMIN=0|false|False|FALSE`.
//! Getting that predicate wrong is expensive in both directions — a build that
//! silently ships no admin UI, or a Rust-only CI job that suddenly needs a Node
//! toolchain — and neither shows up as a compile error, so it is asserted here.
//!
//! `build.rs` is not compiled as a test target by cargo, so it is pulled in here
//! as a module: the file has no dependencies beyond `std`, and this way the
//! function under test is literally the one the build runs, not a copy of it.
//!
//! The module is named `build_rs` rather than `build_script`: this file is itself
//! the `build_script` module of the aggregate test binary, and a module may not
//! share its parent's name.
#[allow(dead_code)]
#[path = "../build.rs"]
mod build_rs;

use self::build_rs::build_requested;

#[test]
fn unset_builds_the_admin_ui() {
    assert!(build_requested(None));
}

#[test]
fn the_four_falsey_spellings_turn_it_off() {
    for value in ["0", "false", "False", "FALSE"] {
        assert!(
            !build_requested(Some(value)),
            "SC_BUILD_ADMIN={value} should disable the UI build"
        );
    }
}

#[test]
fn anything_else_builds() {
    // Including the values that used to be the opt-in, an empty value, and a
    // misspelling: the safe direction for an unrecognised value is the complete
    // binary, because a binary missing its admin UI is the harder failure to spot.
    for value in ["1", "true", "yes", "", "flase", "no", "off"] {
        assert!(
            build_requested(Some(value)),
            "SC_BUILD_ADMIN={value:?} should leave the UI build on"
        );
    }
}

use self::build_rs::recorded_dir;
use std::path::{Path, PathBuf};

#[test]
fn without_a_prefix_the_checkouts_own_bundle_is_recorded() {
    let dist = Path::new("/home/dev/feldspar/ui/admin/dist");
    assert_eq!(recorded_dir(None, "ui/admin", dist), dist.to_path_buf());
}

#[test]
fn a_prefix_re_roots_each_bundle_under_the_install_directory() {
    // What `scripts/build-static.sh` does: the bundles are built in this
    // checkout, but the binary is going to a machine where only the prefix
    // exists, so that is the path it must carry.
    let prefix = Some("/opt/feldspar");
    for (subdir, expected) in [
        ("ui/admin", "/opt/feldspar/ui/admin/dist"),
        ("ui/ide", "/opt/feldspar/ui/ide/dist"),
        ("ui/saltcorn-ui", "/opt/feldspar/ui/saltcorn-ui/dist"),
        ("ui/builder", "/opt/feldspar/ui/builder/dist"),
        ("ui/analytics", "/opt/feldspar/ui/analytics/dist"),
    ] {
        let built = PathBuf::from("/home/dev/feldspar")
            .join(subdir)
            .join("dist");
        assert_eq!(
            recorded_dir(prefix, subdir, &built),
            PathBuf::from(expected)
        );
    }
}

use self::build_rs::recorded_plugins_dir;

#[test]
fn the_bundled_catalog_follows_the_same_prefix_rule() {
    // The modules that ship with the release (`plugins/`) travel beside the two
    // bundles and are found the same way — the checkout's directory for a binary
    // run from its own tree, the install prefix for one being packaged. There is
    // no `dist` under it: what ships is source that npm or pip installs, not
    // something this build produced.
    let plugins = Path::new("/home/dev/feldspar/plugins");
    assert_eq!(recorded_plugins_dir(None, plugins), plugins.to_path_buf());
    assert_eq!(
        recorded_plugins_dir(Some("/opt/feldspar"), plugins),
        PathBuf::from("/opt/feldspar/plugins")
    );
}

#[test]
#[should_panic(expected = "SC_BUNDLE_PREFIX must be an absolute path")]
fn a_relative_plugins_prefix_fails_the_build_too() {
    recorded_plugins_dir(Some("opt/feldspar"), Path::new("/src/plugins"));
}

#[test]
#[should_panic(expected = "SC_BUNDLE_PREFIX must be an absolute path")]
fn a_relative_prefix_fails_the_build() {
    // Rather than recording `opt/feldspar/ui/admin/dist`, which would resolve
    // against whatever directory the service was started in.
    recorded_dir(
        Some("opt/feldspar"),
        "ui/admin",
        Path::new("/src/ui/admin/dist"),
    );
}
