//! Guards the Phase 0 "repo hygiene" artifacts so they cannot silently
//! disappear or lose their required gates. This does not run rustfmt/clippy
//! themselves (those toolchain components may be absent locally and are
//! exercised in CI); it asserts the configuration that drives them exists and
//! declares the pieces the workspace depends on.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from this crate's manifest dir to the workspace root (the ancestor
/// whose `Cargo.toml` declares `[workspace]`).
fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let contents = fs::read_to_string(&manifest).unwrap_or_default();
            if contents.contains("[workspace]") {
                return dir;
            }
        }
        assert!(
            dir.pop(),
            "reached the filesystem root without finding a [workspace] Cargo.toml"
        );
    }
}

fn read(root: &Path, rel: &str) -> String {
    let path = root.join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing {rel}: {e}"))
}

#[test]
fn rustfmt_config_present_and_pins_edition() {
    let root = workspace_root();
    let cfg = read(&root, "rustfmt.toml");
    // Edition must be pinned so `cargo fmt` on a stable toolchain matches the
    // 2024-edition workspace.
    assert!(cfg.contains("edition"), "rustfmt.toml must pin an edition");
    assert!(cfg.contains("2024"), "rustfmt.toml edition should be 2024");
}

#[test]
fn clippy_config_exempts_tests_from_unwrap_lints() {
    let root = workspace_root();
    let cfg = read(&root, "clippy.toml");
    assert!(cfg.contains("allow-unwrap-in-tests"));
    assert!(cfg.contains("allow-expect-in-tests"));
}

#[test]
fn ci_workflow_runs_fmt_clippy_and_test() {
    let root = workspace_root();
    let ci = read(&root, ".github/workflows/ci.yml");
    // The three required gates from the TODO must be wired.
    assert!(ci.contains("cargo fmt"), "CI must gate on rustfmt");
    assert!(ci.contains("cargo clippy"), "CI must gate on clippy");
    assert!(ci.contains("cargo test"), "CI must gate on tests");
}

#[test]
fn gitignore_covers_rust_and_node() {
    let root = workspace_root();
    let ignore = read(&root, ".gitignore");
    // Rust build output.
    assert!(ignore.contains("target"), ".gitignore must ignore target/");
    // Node / frontend output for the React apps served by sc-server.
    assert!(
        ignore.contains("node_modules"),
        ".gitignore must ignore node_modules/"
    );
}

/// Return the body of a TOML table header — everything from `[header]` up to the
/// next line that starts a new table. Used to assert a profile key is set *in
/// the right table*, which a whole-file `contains` cannot tell apart.
fn toml_section<'a>(toml: &'a str, header: &str) -> Option<&'a str> {
    let start = toml.find(header)? + header.len();
    let rest = &toml[start..];
    let end = rest
        .match_indices('\n')
        .find(|(i, _)| rest[i + 1..].starts_with('['))
        .map_or(rest.len(), |(i, _)| i);
    Some(&rest[..end])
}

/// The workspace links a static V8 into every one of its test binaries, so the
/// debug-info budget in the workspace manifest is what keeps
/// `cargo test --workspace` from becoming a burst of ~440 MB links that drives
/// the session into `systemd-oomd`'s kill threshold — which, because oomd kills
/// a *cgroup*, takes the developer's whole terminal with it.
///
/// The budget is two keys, and dropping either puts the memory back without
/// breaking anything a test would notice, so each is asserted by name.
#[test]
fn the_workspace_keeps_debug_info_off_dependencies() {
    let root = workspace_root();
    let manifest = read(&root, "Cargo.toml");

    // Workspace crates keep line tables: a panicking test still prints a
    // backtrace with `file:line`, which is what a test run reads debug info for.
    let dev = toml_section(&manifest, "[profile.dev]")
        .unwrap_or_else(|| panic!("Cargo.toml must declare [profile.dev]"));
    assert!(
        dev.contains(r#"debug = "line-tables-only""#),
        "[profile.dev] should keep line tables only, so test backtraces still \
         carry file:line without paying for full DWARF; found: {dev:?}"
    );

    // Dependencies get none. This is the key that matters: it is the bulk of
    // both the binary size and the linker's peak memory. It needs no
    // counterpart under `[profile.test]` — `test` inherits from `dev`, and that
    // inheritance carries `package."*"` overrides too.
    let header = r#"[profile.dev.package."*"]"#;
    let deps = toml_section(&manifest, header)
        .unwrap_or_else(|| panic!("Cargo.toml must declare {header}"));
    assert!(
        deps.contains("debug = false"),
        "{header} must set `debug = false`: dependency DWARF is what took a test \
         binary to ~440 MB and a --workspace build to ~20 GB of peak memory; \
         found: {deps:?}"
    );
}

/// The budget above is only worth having if it is actually reaching the linker,
/// so this measures the output rather than the setting.
///
/// It deliberately does **not** measure *this* binary: `repo_hygiene` only reads
/// files, so the linker garbage-collects almost everything and it lands around
/// 7 MB whether the budget applies or not — it would pass either way and prove
/// nothing. The binaries that matter are the ones that really do pull in V8, so
/// this looks at the whole `deps/` directory the current build wrote and checks
/// the largest.
///
/// The ceiling is loose on purpose: it only has to separate a build with the
/// budget from one without, not to police ordinary growth in the dependency
/// tree — and that tree has grown a lot. When the budget landed the largest
/// linked binary was ~173 MB with it and ~440 MB without; since the npm module
/// runtime brought in the whole of `deno_runtime` (webgpu, ffi, node crypto,
/// kv, …) rather than bare `deno_core`, the same binary is ~440 MB *with* the
/// budget — its `.text` alone is ~166 MB and its symbol tables another ~140 MB,
/// against ~47 MB of line tables for workspace code. Dependency DWARF is the
/// several hundred megabytes on top of that, so the ceiling moves to 600 MB and
/// still catches a build where the budget stopped applying.
///
/// A partial build (`-p sc-cli` alone) may have linked nothing large yet, in
/// which case there is simply nothing to measure and the test passes — this is a
/// second line of defence behind the manifest assertion above, which is the one
/// that always holds.
#[test]
fn linked_test_binaries_stay_within_the_debug_info_budget() {
    const CEILING_MB: u64 = 600;

    // `<target>/debug/deps/` — derived from this binary rather than assumed, so
    // it follows CARGO_TARGET_DIR and a `--target` build.
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let Some(deps) = exe.parent() else { return };

    let mut largest: Option<(PathBuf, u64)> = None;
    for entry in fs::read_dir(deps).into_iter().flatten().flatten() {
        let path = entry.path();
        // Test binaries have no extension; skip `.rlib`/`.rmeta`/`.d`/`.so`.
        if path.extension().is_some() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if largest.as_ref().is_none_or(|(_, size)| meta.len() > *size) {
            largest = Some((path, meta.len()));
        }
    }

    let Some((path, size)) = largest else { return };
    let size_mb = size / (1024 * 1024);
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    assert!(
        size_mb < CEILING_MB,
        "the largest linked test binary ({name}) is {size_mb} MB, over the \
         {CEILING_MB} MB ceiling. Either the debug-info budget in the workspace \
         Cargo.toml stopped applying, or this run deliberately overrode it \
         (`--config 'profile.dev.package.\"*\".debug=true'`), which is expected \
         to trip this test. Left unfixed, `cargo test --workspace` links every \
         one of these at once and systemd-oomd kills the terminal it runs in."
    );
}

/// The second layer, for a run that overruns anyway: `cargo-guarded.sh` puts
/// cargo in its own memory-capped cgroup, a *sibling* of the terminal's rather
/// than a child, so the cap can only take the build. The two properties worth
/// pinning are that it caps something and that it degrades to plain `cargo`
/// where there is no systemd — a wrapper that silently did nothing on one
/// machine and refused to run on another would be worse than no wrapper.
#[test]
fn the_guarded_cargo_wrapper_caps_memory_and_falls_back() {
    let root = workspace_root();
    let script = read(&root, "scripts/cargo-guarded.sh");

    assert!(
        script.contains("systemd-run") && script.contains("--scope"),
        "the wrapper must run cargo in its own transient scope"
    );
    assert!(
        script.contains("MemoryMax=") && script.contains("MemoryHigh="),
        "the wrapper must set both a throttle (MemoryHigh) and a wall (MemoryMax)"
    );
    assert!(
        script.contains(r#"exec cargo "$@""#),
        "the wrapper must fall back to plain cargo where systemd is unavailable"
    );

    // Executable, or `./scripts/cargo-guarded.sh` in the README does not work.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("scripts/cargo-guarded.sh");
        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{path:?}: {e}"))
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "scripts/cargo-guarded.sh must be executable (mode is {mode:o})"
        );
    }
}

/// The third layer, and the only one that applies without anyone remembering a
/// flag: cargo's default parallelism is the core count, and this workspace's
/// per-job memory is high enough that a 12-core desktop spends 6.6 GB on a
/// workspace rebuild against 3.0 GB at `-j4`. The cap lives in
/// `.cargo/config.toml` so a plain `cargo test` gets it too.
#[test]
fn the_cargo_config_caps_the_default_job_count() {
    let root = workspace_root();
    let cfg = read(&root, ".cargo/config.toml");

    let jobs = cfg
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("jobs"))
        .and_then(|rest| rest.trim_start().strip_prefix('='))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .expect("`.cargo/config.toml` must set a [build] jobs cap");

    assert!(
        (1..=8).contains(&jobs),
        "the job cap is {jobs}: high enough that the memory it exists to bound \
         is back (a rebuild costs roughly 1.2 GB plus 0.45 GB a job), or low \
         enough that it is throttling the build for no reason"
    );
    assert!(
        cfg.contains("[build]"),
        "the cap must be under [build], or cargo ignores it"
    );
}

/// The entry point those layers are meant to be reached through: it sizes both
/// phases from the memory actually free, and runs each of them through the
/// guarded wrapper rather than calling cargo itself.
#[test]
fn the_test_runner_sizes_itself_and_runs_guarded() {
    let root = workspace_root();
    let script = read(&root, "scripts/test.sh");

    assert!(
        script.contains("MemAvailable"),
        "the runner must size its budget from free memory, not from nproc"
    );
    assert!(
        script.contains("cargo-guarded.sh"),
        "the runner must go through the guarded wrapper, so an overrun can only \
         take the test run"
    );
    for dial in ["--test-threads", "-j"] {
        assert!(
            script.contains(dial),
            "the runner must cap {dial}, which is the phase it bounds"
        );
    }
    // Both overrides are documented; a runner whose dials cannot be forced is
    // one that gets abandoned the first time its arithmetic is wrong.
    for var in ["SC_TEST_MEM", "SC_TEST_JOBS", "SC_TEST_THREADS"] {
        assert!(script.contains(var), "{var} must remain an override");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("scripts/test.sh");
        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{path:?}: {e}"))
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "scripts/test.sh must be executable (mode is {mode:o})"
        );
    }
}

/// The markdown documents the documentation set consists of: the top-level
/// entry points plus everything in `docs/`.
fn documentation_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["README.md", "TODO.md", "CLAUDE.md"]
        .iter()
        .map(|rel| root.join(rel))
        .filter(|p| p.is_file())
        .collect();
    let docs = root.join("docs");
    let entries = fs::read_dir(&docs).unwrap_or_else(|e| panic!("missing docs/: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
    assert!(
        out.iter().any(|p| p.ends_with("docs/TECHNICAL_DESIGN.md")),
        "the technical design document must be part of the documentation set"
    );
    out
}

/// Every markdown link to a `.md` file, in every document, must resolve to a
/// file that exists — a tutorial that links a sibling that was renamed or never
/// written is a broken promise the reader discovers instead of a test.
///
/// Only `.md` targets are checked (external URLs, anchors and code snippets that
/// merely look like links are left alone), which is exactly the class of links
/// the docs use to refer to each other.
#[test]
fn documentation_links_resolve() {
    let root = workspace_root();
    for doc in documentation_files(&root) {
        let text = fs::read_to_string(&doc).unwrap_or_else(|e| panic!("{doc:?}: {e}"));
        let dir = doc.parent().unwrap_or(&root);
        // Markdown inline links: `[label](target)`. Scan for `](` and take the
        // target up to the closing parenthesis.
        for (idx, _) in text.match_indices("](") {
            let rest = &text[idx + 2..];
            let Some(end) = rest.find(')') else { continue };
            let target = &rest[..end];
            // Strip a `#fragment`; skip external URLs and non-.md targets.
            let path_part = target.split('#').next().unwrap_or("");
            if path_part.contains("://") || !path_part.ends_with(".md") {
                continue;
            }
            let resolved = dir.join(path_part);
            assert!(
                resolved.is_file(),
                "{}: broken link `{target}` (resolved to {resolved:?})",
                doc.display()
            );
        }
    }
}

/// The tutorials link to each other, so a reader who finishes one finds the
/// next: the React tutorial leads to the file-fields tutorial, which leads back
/// and on to the ownership tutorial, which builds on it.
#[test]
fn tutorials_are_cross_linked() {
    let root = workspace_root();
    let react = read(&root, "docs/tutorial-react-todo.md");
    assert!(
        react.contains("tutorial-file-fields.md"),
        "the React tutorial should point at the file-fields tutorial as a next step"
    );
    let files = read(&root, "docs/tutorial-file-fields.md");
    assert!(
        files.contains("tutorial-react-todo.md"),
        "the file-fields tutorial builds on the React tutorial and should link it"
    );
    assert!(
        files.contains("tutorial-ownership.md"),
        "the file-fields tutorial should point at the ownership tutorial as a next step"
    );
    let ownership = read(&root, "docs/tutorial-ownership.md");
    assert!(
        ownership.contains("tutorial-file-fields.md"),
        "the ownership tutorial builds on the file-fields tutorial and should link it"
    );
    assert!(
        ownership.contains("tutorial-triggers.md"),
        "the ownership tutorial should point at the triggers tutorial as a next step"
    );
    let triggers = read(&root, "docs/tutorial-triggers.md");
    assert!(
        triggers.contains("tutorial-ownership.md"),
        "the triggers tutorial builds on the ownership tutorial and should link it"
    );
    assert!(
        triggers.contains("tutorial-python.md"),
        "the triggers tutorial should point at the Python tutorial, which is its step 5 in \
         the other language"
    );
    let python = read(&root, "docs/tutorial-python.md");
    assert!(
        python.contains("tutorial-triggers.md"),
        "the Python tutorial builds on the triggers tutorial and should link it"
    );
    assert!(
        python.contains("tutorial-modules.md"),
        "the Python tutorial should link the modules tutorial, which is its other half"
    );
    let modules = read(&root, "docs/tutorial-modules.md");
    assert!(
        modules.contains("tutorial-python.md"),
        "the modules tutorial covers one of two languages and should link the other"
    );
    assert!(
        triggers.contains("tutorial-agents.md"),
        "the triggers tutorial should point at the agents tutorial as a next step"
    );
    assert!(
        triggers.contains("tutorial-workflows.md"),
        "the triggers tutorial should point at the workflows tutorial as a next step"
    );
    assert!(
        triggers.contains("tutorial-models.md"),
        "the triggers tutorial should point at the models tutorial, whose `predict(\"…\")` \
         is one more function of the formula language a trigger's formulas use"
    );
    let models = read(&root, "docs/tutorial-models.md");
    assert!(
        models.contains("tutorial-triggers.md"),
        "the models tutorial builds on the triggers tutorial and should link it"
    );
    assert!(
        models.contains("tutorial-python.md"),
        "the models tutorial's second provider comes from a Python module, so it should link \
         the Python tutorial"
    );
    let workflows = read(&root, "docs/tutorial-workflows.md");
    assert!(
        workflows.contains("tutorial-triggers.md"),
        "the workflows tutorial builds on the triggers tutorial and should link it"
    );
    assert!(
        workflows.contains("tutorial-agents.md"),
        "the workflows tutorial should point at the agents tutorial as a next step"
    );
    let agents = read(&root, "docs/tutorial-agents.md");
    assert!(
        agents.contains("tutorial-triggers.md"),
        "the agents tutorial builds on the triggers tutorial and should link it"
    );
    assert!(
        agents.contains("tutorial-graphql.md"),
        "the agents tutorial should point at the GraphQL tutorial as a next step"
    );
    let graphql = read(&root, "docs/tutorial-graphql.md");
    assert!(
        graphql.contains("tutorial-react-todo.md"),
        "the GraphQL tutorial builds on the React tutorial and should link it"
    );
    assert!(
        graphql.contains("tutorial-ownership.md"),
        "the GraphQL tutorial leans on the ownership rules and should link them"
    );
    assert!(
        graphql.contains("tutorial-rest-queries.md"),
        "the GraphQL tutorial should point at the REST-queries tutorial as a next step"
    );
    let rest = read(&root, "docs/tutorial-rest-queries.md");
    assert!(
        rest.contains("tutorial-react-todo.md"),
        "the REST-queries tutorial builds on the React tutorial and should link it"
    );
    assert!(
        rest.contains("tutorial-constraints.md"),
        "the REST-queries tutorial should point at the constraints tutorial as a next step"
    );
    let constraints = read(&root, "docs/tutorial-constraints.md");
    assert!(
        constraints.contains("tutorial-ownership.md"),
        "the constraints tutorial is the other half of `the database decides` and should \
         link the ownership tutorial"
    );
    assert!(
        rest.contains("tutorial-ownership.md"),
        "the REST-queries tutorial leans on the ownership rules and should link them"
    );
    assert!(
        rest.contains("tutorial-graphql.md"),
        "the REST-queries tutorial should point at the questions GraphQL answers instead"
    );
}

/// The agents tutorial has to teach **the whole loop**, because every step of it
/// is a screen an admin has to be able to find — and a tutorial that quietly
/// lost one of them would still read fine. Each fragment below is one step:
/// connect a provider, give an agent a table, watch a tool call happen, hand it
/// a trigger, point it at code, and hang it off a trigger of its own.
#[test]
fn the_agents_tutorial_teaches_each_step_of_the_loop() {
    let root = workspace_root();
    let agents = read(&root, "docs/tutorial-agents.md");
    for fragment in [
        "Test connection",             // the provider, checked before it is saved
        "query_table",                 // the grant that lets an agent read a table
        "query_tasks",                 // …and the tool name its configuration derives
        "tool call",                   // what the transcript shows happening
        "Stop",                        // the abort, mid-answer
        "History",                     // …and where the run is afterwards
        "run_trigger",                 // the grant that lets an agent act
        "min_role",                    // …still gated by the trigger's own floor
        "`coding`",                    // the one trait the whole coding loop is
        "search_files",                // …grep,
        "edit_file",                   // …edit,
        "build_application",           // …build, and read the diagnostics
        "May create and change files", // the checkbox the edits are behind
        "no shell",                    // …and the script grant that ships instead of one
        "run_agent",                   // the agent as a trigger body (§11.5)
        "template literal",            // …whose prompt is a formula, written the safe way
        "max_steps",                   // the seatbelt
        "sentinel",                    // the redacted key
        "describe_action",             // …how it finds out what an action takes
        "save_trigger",                // …and writes the trigger that runs it
    ] {
        assert!(
            agents.contains(fragment),
            "the agents tutorial should cover `{fragment}`"
        );
    }
}

/// The workflows tutorial has to teach **the whole engine**, because each of
/// these is something a workflow author is stuck without and a document that
/// quietly lost one would still read fine: the five step kinds, the two ways a
/// run stops, the two ways it is answered, versioning, and — the one the engine
/// asks of *them* — what to do about a step that is not idempotent.
#[test]
fn the_workflows_tutorial_teaches_each_part_of_the_engine() {
    let root = workspace_root();
    let workflows = read(&root, "docs/tutorial-workflows.md");
    for fragment in [
        "A workflow",                   // the trigger body that makes one
        "only_if",                      // …and the condition that stops it starting itself
        "run_js_code",                  // an Action step, and the one that does the reading
        "For each",                     // the loop,
        "Item name",                    // …and how its body names the item
        "User form",                    // the wait for a person,
        "Give up after",                // …and the deadline that makes abandoning it a decision
        "Answers go to",                // …and where the answers land
        "Branch on a condition",        // control flow as data
        "Save a new version",           // versions are appended
        "Restore",                      // …and a revert is a new one
        "pinned",                       // …which is what a suspended run finishes on
        "Restart the server",           // durability, demonstrated rather than claimed
        "at least once",                // the guarantee,
        "idempotent",                   // …and the section about living with it
        "Retry, then fall through",     // the error policies
        "context.error",                // …and what a handler reads
        "Cancel",                       // the two buttons a stuck run has
        "Retry from",                   //
        "unknown identifier `context`", // what naming the run outside one says (§10.3)
    ] {
        assert!(
            workflows.contains(fragment),
            "the workflows tutorial should cover `{fragment}`"
        );
    }
}

/// The Python tutorial has to teach **the whole of the second language**, and
/// two of its fragments are obligations rather than topics: an admin who installs
/// a Python module gets no sandbox, and a version change is not live until a
/// restart. Both are said on the Modules tab, and a tutorial that quietly lost
/// either would still read fine — which is exactly why they are pinned here.
#[test]
fn the_python_tutorial_teaches_the_language_and_says_what_it_costs() {
    let root = workspace_root();
    let python = read(&root, "docs/tutorial-python.md");
    for fragment in [
        "Settings → Development",    // which of the four states this process is in
        "--features python",         // …and the rebuild that is the only way into it
        "run_python_code",           // the action a body is configured on
        "Nothing is awaited",        // the one deep difference from the JS body
        "db.tasks",                  // the five surfaces, each at least once
        "fetch(",                    //
        "fs(\"uploads\")",           //
        "trigger(\"archive_done\")", //
        "modfn",                     //
        "import subprocess",         // the gate, refusing the author's own import
        "hygiene, not a sandbox",    // …and its honest account of itself
        "saltcorn.Timeout",          // the deadline a bare `except Exception` must not eat
        "1000 rows",                 // the bounds a body runs inside
        "saltcorn.plugins",          // the plugin: how a distribution advertises one
        "@sc.action",                // …the three things it can supply,
        "@sc.function",              //
        "@sc.table_provider",        //
        "inspect.signature",         // …the parameter rule that is better than v1's
        "sc.Field.string",           // …and the field vocabulary its settings speak
        "Python — local directory",  // installing one, in the words the form uses
        "--python-bin",              // …and the ABI trap that flag is the repair for
        "There is no sandbox",       // the first sentence an admin must read
        "at the next restart",       // …and the second
    ] {
        assert!(
            python.contains(fragment),
            "the Python tutorial should cover `{fragment}`"
        );
    }
}

/// Python is the one capability a **stock build does not have**, so the three
/// documents an operator reads have to say the same thing about it: the feature
/// is a rebuild, the shipped tarball has none, and there is no flag that adds it.
/// An operator who reads "off by default" and goes looking for a flag will not
/// find one.
#[test]
fn the_readme_states_the_python_build_line() {
    let root = workspace_root();
    let readme = read(&root, "README.md");
    for fragment in [
        "--features python",       // the rebuild, spelled out
        "python3-dev",             // …and what it links against
        "libpython",               // why it is a build-time decision at all
        "docs/tutorial-python.md", // where an admin goes next
        "--python-max-inflight",   // the runtime knobs, in the options table
        "--python-bin",            //
    ] {
        assert!(
            readme.contains(fragment),
            "the README should state `{fragment}` about Python"
        );
    }
    // The static artifact and the Python build are mutually exclusive, and §4.1
    // is where somebody deciding how to deploy will look for it.
    assert!(
        readme.contains("why it has no Python"),
        "§4.1 should say that the packaged static artifact carries no Python"
    );
}

/// §15 of the design document is the code-adapter section, and since the Python
/// milestone it describes **two** adapters. The old `CodeAdapter` sketch
/// (`call(module, func, args)` + `register(decl)`) was superseded by what was
/// built, and a document still carrying it would send a reader looking for a
/// trait that does not exist.
#[test]
fn the_design_document_describes_both_code_adapters() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    assert!(
        design.contains("### 15.2 Python"),
        "§15 should have a Python subsection beside §15.1's modules"
    );
    assert!(
        !design.contains("async fn call(&self, module: &str, func: &str"),
        "§15's superseded `CodeAdapter` sketch should be gone, not sitting beside the built one"
    );
    for fragment in [
        "async fn run_code(&self, call: CodeCall<'_>)", // the trait as built
        "Python::detach",                               // the GIL claim, with its mechanism
        "PyThreadState_SetAsyncExc",                    // …and what stops a run
        "There is no sandbox",                          // §10's obligation, in the design
        "a restart is the guarantee",                   // §11's reload semantics
        "+crt-static",                                  // why the feature is off by default
    ] {
        assert!(design.contains(fragment), "§15 should carry `{fragment}`");
    }
}

/// The React tutorial is where an admin learns the edit loop, and since the IDE
/// milestone that loop is **format → fix a type error → build**, in the
/// workbench rather than in the file manager's textarea. Each fragment below is
/// a step of it that would be invisibly lost if the section were rewritten
/// around the old file-manager loop: the tutorial would still read fine.
#[test]
fn the_react_tutorial_teaches_the_ide_loop() {
    let root = workspace_root();
    let react = read(&root, "docs/tutorial-react-todo.md");
    for fragment in [
        "/ide/?store=apps",     // how the workbench is reached, and from where
        "Format Document",      // …prettier, by the command's own name
        ".prettierrc",          // …with the project's own configuration
        "editor.formatOnSave",  // …and on save
        "TasksRow",             // the type a type error is caught against
        "Problems",             // where a diagnostic lands, from either source
        "node_modules",         // …which is why semantics need a build first
        "Saltcorn: Build Appl", // the build, from inside the editor
        "Source Control",       // and committing what was just edited
    ] {
        assert!(
            react.contains(fragment),
            "the React tutorial should cover `{fragment}`"
        );
    }
}

/// §12.1 is the IDE's design section, and the milestone deviated from it in
/// places that are load-bearing — a build that is served without a flag, a
/// language client that is not `monaco-languageclient`, an SCM view that is
/// deliberately a subset. A design document that still described the plan
/// instead of what was built would mislead the next person to read it, which is
/// the failure this test exists to catch.
#[test]
fn the_design_records_what_the_ide_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        "IDE_CONTENT_SECURITY_POLICY",        // the relaxed policy, by name
        "SC_BUILD_ADMIN=0",                   // …and no `--ide-dir` to decide
        "before `initialize`",                // the ordering the contributions depend on
        "monaco-languageclient` is not used", // the language client deviation
        "does not match the server's root",   // …and the URI bridge it forced
        "close frame",                        // where a refusal is carried
        "Source control: the SCM view, with an index", // the subset, and
        "Left out",                           // …what it leaves out
        "a diff against nothing",             // …for a stated reason
        "Staged Changes",                     // the index, which is in
        "staged_only",                        // …and the flag that is the whole of it
    ] {
        assert!(
            design.contains(fragment),
            "§12.1 should record `{fragment}`"
        );
    }
}

/// §11 is the agents milestone's design section, and the milestone deviated from
/// it in places that are load-bearing — a stop reason no vendor sends, a socket
/// protocol that settled differently, a trait signature that had to grow a
/// catalog, a seam that moved a layer down. The last two milestones proved this
/// is the step that is easy to skip and expensive to skip: a design document
/// that still described the plan would mislead the next person to read it.
///
/// Each phase records its own deviations, so this asserts that every phase's
/// block is still there and still names the thing that surprised it.
#[test]
fn the_design_records_what_the_agents_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // Every phase writes its deviations down under the same heading.
        "**What was built, where it deviates from the above** (Phase 1",
        "**What was built, where it deviates** (Phase 2)",
        "**What was built, where it deviates** (Phase 3, `query_table`)",
        "**What was built, where it deviates** (Phase 3, the write traits)",
        "**What was built, where it deviates** (Phase 3, `run_trigger`)",
        "**What was built, where it deviates** (Phase 4)",
        "**What was built, where it deviates** (Phase 5, the coding traits)",
        "**What was built, where it deviates from the above** (Phase 6)",
        // §11.1: the rig APIs that turned out to be wrong or missing.
        "`StopReason` has two variants", // no vendor sends one when streaming
        "merges consecutive tool results", // a wire-format obligation
        "defaults `max_tokens` to 4096", // …and a vendor requirement
        "`reqwest` moved to 0.13",       // one HTTP client in the build
        // §11.2: the extension point as it settled.
        "`AgentTrait::tools` takes the catalog",
        "`RunCaller` has two shapes and no default",
        // §11.4: the socket protocol as it settled.
        "The socket protocol, as it settled",
        "A tool call is emitted once",
        "answered with **silence**",
        // §11.3: delegation, added after the milestone closed.
        "**What was built, where it deviates** (`subagent`)",
        "Delegation, not handoff",
        "`TraitContext` carries a `Delegator`",
        "A cycle is refused by name, a chain by number",
        "Nothing came back means the delegation failed",
        // §11.5: the agent as a trigger body.
        "`ProviderConnector` moved down to `sc-agent`",
        "registered apart from the built-in action set",
        "triggered run is given no trigger dispatcher",
        // §11.3: the trigger half of `admin_copilot`, and the decision it turns
        // on — an action's settings are fetched when the model asks, not filled
        // in by a second, hidden inference call the way Saltcorn 1 did it.
        "**The triggers, and the problem they pose.**",
        "progressive disclosure inside the one loop",
        "A hidden second inference is a run nobody can read",
        "The four grants cover both halves",
        "`save_trigger` takes one trigger, not a list",
    ] {
        assert!(design.contains(fragment), "§11 should record `{fragment}`");
    }
}

/// The triggers tutorial teaches the three things Phases 2–8 built, and each of
/// them is a *screen* an admin has to be able to find: an `only_if` on a table
/// event, a `none` trigger reached through an application's API, and a periodic
/// one. A tutorial that quietly lost a third of that would still read fine,
/// which is exactly why it is worth a test.
#[test]
fn the_triggers_tutorial_covers_all_three_kinds_of_trigger() {
    let root = workspace_root();
    let triggers = read(&root, "docs/tutorial-triggers.md");
    for fragment in [
        "Only if",       // the per-row condition on a table event
        "old.done",      // …which is what makes it "became done" rather than "is done"
        "insert_row",    // the audit write
        "no event",      // the `none` kind, as the event picker spells it
        "/api/actions/", // reached through the app's API
        "Minimum role",  // guarded by the trigger's own floor
        "Once a day",    // the periodic kind, as the picker spells it
        "UTC",           // …which is the thing people get wrong
    ] {
        assert!(
            triggers.contains(fragment),
            "the triggers tutorial should cover `{fragment}`"
        );
    }
}

/// §13.4 is the GraphQL milestone's design section, and the parts worth having
/// written down are the ones somebody would otherwise have to read the provider
/// to learn: what the wire contract looks like, that the aggregate is *not*
/// implemented here but lowered onto `sc-expr`, the four authorization rules an
/// aggregate and a projected key made necessary, and the four bounds one
/// operation is served under. A section that lost any of them would still read
/// like a description of a GraphQL API, which is exactly why it is worth a test.
#[test]
fn the_design_records_what_the_graphql_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // The wire contract: two endpoints, one schema, the legacy HTTP rule.
        "POST {mount}",
        "schema.graphql",
        "200 with an `errors` array",
        "BigInt",                  // …and a scalar that does not silently lose information
        "FileValue { path, url }", // …nor become a second download path
        // Names are derived, and an underivable one is an omission.
        "<child>_by_<key>",
        "omitted with",
        // The load-bearing decision: the provider does not aggregate.
        "_fd_g1",
        "count(distinct: Column)",
        "row_number() OVER (PARTITION BY …)",
        // The four authorization rules.
        "ownership::aggregate_values_as",
        "ownership::join_guard",
        "never a quiet zero",
        "nullable", // …which is why a child list field is
        // What one operation may cost, and where each bound is counted.
        "max_complexity",
        "statement_budget",
        "statement is issued", // …two of which refuse before one ever is
        // …and the screen that drives it with the admin's own authority.
        "runApplicationGraphql",
    ] {
        assert!(
            design.contains(fragment),
            "§13.4 should record `{fragment}`"
        );
    }
}

/// The GraphQL tutorial has to reach the milestone's own query and then keep
/// going past the happy path: an admin is the one caller no rule applies to, so
/// a tutorial that stopped at "it works" would teach a GraphQL API with no
/// authorization and no cost. Each fragment below is one thing a reader would
/// otherwise have to discover in production.
#[test]
fn the_graphql_tutorial_reaches_the_motivating_query_and_its_rules() {
    let root = workspace_root();
    let tutorial = read(&root, "docs/tutorial-graphql.md");
    for fragment in [
        "employees_aggregate(where:", // the query the milestone exists for
        "_fd_g1",                     // …and the correlated subquery it becomes
        "row_number()",               // a nested `limit` is per parent
        "DataLoader",                 // …and a child list is one statement per level
        "insert_employees",           // the write path
        "BAD_USER_INPUT",             // …and what a refusal carries
        "x-csrf-token",               // calling it without a browser
        "errors in the body",         // …where a GraphQL endpoint puts its refusals
        "gql.tada",                   // typing it in the app, with no codegen step
        "partial results",            // what a caller who is not an admin sees
        "quiet zero",                 // …and the refusal that is never a count
        "32",                         // the statement budget, in the limits table
    ] {
        assert!(
            tutorial.contains(fragment),
            "the GraphQL tutorial should cover `{fragment}`"
        );
    }
}

/// The API milestone spans three sections — query parameters in the endpoint
/// model (§13.1), the generated directory's contract (§13.3), and the REST query
/// string, custom SQL queries and per-provider configuration (§13.4) — and each
/// records something a reader would otherwise have to find in the source: the
/// data structure a filter vocabulary forced, the boundary between the generated
/// directory and the developer's project, the stated subset of PostgREST's
/// grammar, and the authority a raw statement does *and* does not carry.
#[test]
fn the_design_records_what_the_api_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // §13.1: a query parameter is part of the endpoint, and what that emits.
        "pub query:   Vec<QueryParam>",
        "A query parameter is part of the endpoint",
        "URLSearchParams",   // …which does the encoding, not concatenation
        "query_all",         // …and the two questions a handler asks
        "dropped predicate", // the failure a `HashMap` would hide
        // §13.3: the generated directory's contract, stated in the tree.
        "DatabaseDriver::render_ddl", // schema.sql is the driver's, not a second writer's
        "`AGENTS.md` goes at the project root",
        "emit_app_client",         // one re-emit, three callers
        "logged and never fatal",  // …inside somebody else's schema change
        "updateApplicationClient", // …and the same thing on demand
        // §13.4: configuration, the query string, and its stated subset.
        "ApiProviderInfo::config_spec",
        "supports_custom_queries",
        "syntax over the read layer",
        "one-to-many embeds",
        "ownership::join_guard",
        "reserved words a column",
        // §13.4: custom SQL, the one escape hatch, and its authority.
        "Statement::Raw { sql, binds }",
        "rewrite_named_params",
        "DatabaseDriver::describe",
        "READ ONLY",
        "impossible to save",
        "describeCustomQuery",
    ] {
        assert!(
            design.contains(fragment),
            "the design should record `{fragment}`"
        );
    }
}

/// The REST tutorial has to reach the milestone's own query and then keep going
/// past the happy path, for the same reason the GraphQL one does: an admin is the
/// caller no rule applies to, and a custom query is a hole somebody opens on
/// purpose. Each fragment below is one thing a reader would otherwise discover in
/// production.
#[test]
fn the_rest_tutorial_reaches_the_motivating_query_and_its_rules() {
    let root = workspace_root();
    let tutorial = read(&root, "docs/tutorial-rest-queries.md");
    for fragment in [
        // The query the milestone exists for, and every piece of its vocabulary.
        "select=title,published,author(name,country)",
        "is_null.true",
        "in.(200,300,400)",
        "order=published.desc,title",
        "Row cap per list read", // …and the ceiling an absent `limit` becomes
        // Nothing is ignored: the refusals, by the name of the thing refused.
        "one-to-many embeds",
        "!inner",
        "rows the caller did not ask for",
        // Reads that reach a second table are still reads.
        "read of the table it reaches",
        // The typed client, which is why a query parameter is in the endpoint.
        "ListBooksQuery",
        "Record<string, string>",
        "useQuery",
        // A custom query: written, checked, and what it is a hole in.
        "describeCustomQuery",
        "no table event",
        "READ ONLY",
        "feldspar api add-query",
        "list-queries",
        "drop table books", // …an argument is a value, and the table survives
        "TopAuthorsResponse",
        // And the generated directory that keeps up with all of it.
        "schema.sql",
        "AGENTS.md",
        "Update code",
    ] {
        assert!(
            tutorial.contains(fragment),
            "the REST tutorial should cover `{fragment}`"
        );
    }
}

/// The workflow milestone, held to what it built (§10.3): the four things the
/// engine *is*, the guarantee that changed on contact with reality, and the two
/// rules a reader is most harmed by losing — what a step's formulas may name,
/// and what one advance is, which is what a loop's durability rests on.
#[test]
fn the_design_records_what_the_workflow_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // A workflow is a trigger body, and its steps are versioned rows.
        "TriggerBody",
        "_fd_workflow_versions",
        "append-only",
        "subject_version",
        // Control flow is data, and the step set is five.
        "Control flow is data",
        "Five step kinds",
        "UserForm",
        // The machine, and what one advance guarantees.
        "no IO",
        "Each step runs in one transaction, and one advance is one atomic write",
        "SharedTx",
        "at least once",
        // The queue, and why it is not the bus yet.
        "WorkQueue",
        "Recovery is not a special case",
        "started by `serve`",
        // The scope rule: one shape, and both the callers it is handed to.
        "workflow_shape",
        "step_shape",
        "ConfigCheck::shape",
        "ActionContext::with_run_context",
        // …and what one advance is, which is what makes a loop's item durable.
        "One advance services one *step entry*",
        "durability granularity of a loop is the item",
        // The editor, and what was deliberately not built.
        "React Flow",
        "WorkflowRoom",
    ] {
        assert!(
            design.contains(fragment),
            "the design should record `{fragment}`"
        );
    }
}

/// The constraints milestone, held to what it built (§5.1): where a constraint
/// lives, what enforces a row constraint, and the two rules that are refusals
/// rather than features.
#[test]
fn the_design_records_what_the_constraints_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // Where a constraint lives, and why there is no table for it.
        "no `_fd_constraints`",
        "saltcorn_constraint",
        "AddUniqueConstraint",
        // What enforces a row constraint, and the shape of the generated body.
        "CONSTRAINT TRIGGER",
        "(SELECT (NEW).*)",
        "DEFERRABLE INITIALLY IMMEDIATE",
        "ERRCODE = 'check_violation'",
        // …and how the admin's own sentence gets back to the caller.
        "(constraint \"<name>\")",
        // The names, and the two refusals the schema editor owns.
        "sc_uq_<table>_<fields>",
        "dropped from under it",
        "rebuilt whenever its text fields change",
    ] {
        assert!(
            design.contains(fragment),
            "the design should record `{fragment}`"
        );
    }
}

/// The i18n milestone, held to what it built (§16.1). The section has to carry
/// the things a reader would otherwise have to reconstruct from four crates:
/// the three populations and which of them is out of scope, the catalogue's
/// shape and its one load-bearing decision, the format and the fixture that
/// keeps its two implementations honest, the four domains and the two homes an
/// application's catalogue has, the negotiation order, the rule that the server
/// translates what the server says, and what is deliberately left in English.
#[test]
fn the_design_records_what_the_i18n_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // The section itself, and the three populations.
        "### 16.1 Internationalisation",
        "who wrote the string and when",
        // The catalogue: the key is the English, and the two shapes of a value.
        "keyed by the English source text",
        "CLDR plural category",
        "`en.json`**: the key is the English",
        "orphans the translation",
        // The format, and the reason it is not the template language.
        "renders as written",
        "crates/sc-i18n/fixtures/format.json",
        "positional `%s` survives inside the Saltcorn UI shim only",
        // The domains, and where an application's catalogue lives.
        "`<project>/locales/{locale}.json`",
        "_fd_translations",
        "`CatalogStore`",
        "**sparse values in",
        // Negotiation: the order, and the rule that keeps it out of a global.
        "the `lang` cookie",
        "`pt-BR` → `pt` → default",
        "Vary: Accept-Language, Cookie",
        "never ambient",
        "Zero cost when unused",
        // D5, and where the spec walk actually lives.
        "The server translates everything the server says",
        "`sc_types::translate_spec",
        // Extraction, the lint and the CLI.
        "tree-sitter",
        "error naming file and",
        "feldspar i18n extract | lint | check | translate",
        "Coverage is a number, not a gate",
        // The translator seam, and the check that is not in the prompt.
        "`Translator` is declared",
        "**not in the prompt**",
        "`{nombre}`",
        // The two runtimes an application gets, and the Saltcorn UI half.
        "generated, not depended on",
        "ViewRuntime::strings_for_i18n",
        "async-local context inside the worker",
        // What stays English.
        "What is deliberately not translated",
        "System errors",
        "negotiated, not routed",
    ] {
        assert!(
            design.contains(fragment),
            "the design should record `{fragment}`"
        );
    }
}

/// The i18n tutorial has to walk the milestone's own definition of done — two
/// locales turned on, the admin UI in French, a React application translated end
/// to end, the same for a Saltcorn UI application — and then say the two things
/// a reader would otherwise meet in production: that the first argument to `t()`
/// must be a literal, and that a translation is live without a rebuild. Each
/// fragment below is one step or one rule that would be invisibly lost if the
/// page were rewritten, because the tutorial would still read fine without it.
#[test]
fn the_i18n_tutorial_walks_the_definition_of_done() {
    let root = workspace_root();
    let tutorial = read(&root, "docs/tutorial-i18n.md");
    for fragment in [
        // Turning it on, and the language picker that appears when you do.
        "Settings → Localisation",
        "`en, fr`",
        "language select appears in the account row",
        "`language` column",
        // The two catalogues behind "the admin UI in French", which ship
        // differently — the reason this tutorial has a command in it.
        "feldspar i18n translate --domain admin --locale fr",
        "the server translates everything the server",
        // Negotiation, from the outside.
        "content-language: fr",
        "vary: accept-language, cookie",
        // The call shapes, and the rule a reader is most harmed by losing.
        "must be a string literal",
        "renders as written",
        "tc(\"verb\", \"Order\")",
        "<T>",
        // The screen, the button, and the check that is not in the prompt.
        "Applications → your application → Translations",
        "Translate missing",
        "rejected and left in English",
        "No longer used",
        "Not wrapped in `t()`",
        // Where it was written, and that it is live without a build.
        "locales/fr.json",
        "with no rebuild",
        "/i18n/fr.json",
        // The command line, and what CI fails on.
        "feldspar i18n lint",
        "feldspar i18n check",
        "Coverage is reported, not",
        // The Saltcorn UI half, and its two differences.
        "_fd_translations",
        "getStringsForI18n",
        "`%s` is preserved",
        // And what stays English.
        "System errors",
        "negotiated, not routed",
    ] {
        assert!(
            tutorial.contains(fragment),
            "the i18n tutorial should cover `{fragment}`"
        );
    }
}

/// The i18n tutorial is reachable from the two tutorials whose applications it
/// translates — a page nothing links to is a page nobody finds.
#[test]
fn the_i18n_tutorial_is_linked_from_the_applications_it_translates() {
    let root = workspace_root();
    for doc in [
        "docs/tutorial-react-todo.md",
        "docs/tutorial-saltcorn-ui.md",
    ] {
        assert!(
            read(&root, doc).contains("tutorial-i18n.md"),
            "{doc} should link to the i18n tutorial"
        );
    }
}

/// The constraints tutorial has to reach all four kinds *and* the two things a
/// reader would otherwise meet in production: a rule refusing a write that never
/// went near Saltcorn, and a formula refused for asking a question the database
/// cannot answer.
#[test]
fn the_constraints_tutorial_covers_all_four_kinds_and_their_refusals() {
    let root = workspace_root();
    let tutorial = read(&root, "docs/tutorial-constraints.md");
    for fragment in [
        // The four kinds, by the name the screen calls them.
        "Jointly unique",
        "Full-text search",
        "Row constraint",
        "sc_uq_books_author_title",
        // The message, which is the whole reason the form asks for one.
        "You already have a book by that title.",
        // A formula that reaches another table — the case a CHECK cannot do.
        "authorⱵname",
        "booksↃauthor.length",
        // …and the two it may not ask.
        "the database has no session",
        "_insert",
        // Enforced where it counts.
        "INSERT INTO books",
        // Read back rather than stored, including somebody else's.
        "External",
        "no second copy",
        // The refusals the schema editor owns, and the deferral.
        "It is refused, by",
        "SET CONSTRAINTS ALL DEFERRED",
    ] {
        assert!(
            tutorial.contains(fragment),
            "the constraints tutorial should cover `{fragment}`"
        );
    }
}

/// Read one `[section]` of a `Cargo.toml`, returning the `sc-*` keys declared in
/// it. Deliberately a line scanner rather than a TOML parse: the manifests use
/// one dependency per line in both `sc-foo.workspace = true` and
/// `sc-foo = { … }` spellings, and this test has no business pulling a parser in
/// to read them.
fn manifest_section_sc_keys(manifest: &str, section: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == format!("[{section}]");
            continue;
        }
        if !inside || !trimmed.starts_with("sc-") {
            continue;
        }
        let key: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !key.is_empty() {
            keys.push(key);
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Every crate under `crates/`, with the `sc-*` crates it depends on directly
/// (dev-dependencies excluded — a dev-only edge is not part of the layering).
fn workspace_dependency_graph(root: &Path) -> Vec<(String, Vec<String>)> {
    let mut graph = Vec::new();
    let entries =
        fs::read_dir(root.join("crates")).unwrap_or_else(|e| panic!("missing crates/: {e}"));
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("Cargo.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("unreadable {}: {e}", manifest_path.display()));
        let name = manifest
            .lines()
            .find_map(|l| l.trim().strip_prefix("name = \""))
            .and_then(|l| l.strip_suffix('"'))
            .unwrap_or_else(|| panic!("no package name in {}", manifest_path.display()))
            .to_owned();
        graph.push((name, manifest_section_sc_keys(&manifest, "dependencies")));
    }
    graph.sort();
    graph
}

/// The body of the `n`th fenced ```mermaid block in `doc`.
fn mermaid_block(doc: &str, n: usize) -> String {
    doc.split("```mermaid\n")
        .skip(1)
        .map(|rest| {
            rest.split_once("```")
                .unwrap_or_else(|| panic!("an unterminated ```mermaid block"))
                .0
                .to_owned()
        })
        .nth(n)
        .unwrap_or_else(|| panic!("the design document has no mermaid block {n}"))
}

/// §2's crate diagram and the dependency table under it are the picture of the
/// layering, and a picture that has drifted from `Cargo.toml` is worse than no
/// picture: it is read and believed. So the table is checked against the
/// manifests column for column, and every arrow in the diagram is checked to be
/// a dependency that actually exists.
///
/// The diagram is the graph's *transitive reduction*, so it is asserted to be a
/// subset of the real edges, not equal to them; the table carries the complete
/// lists, and that is what equality is asserted on.
#[test]
fn the_design_crate_diagram_matches_the_workspace_manifests() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    let actual = workspace_dependency_graph(&root);

    // The documented table: rows of "| `sc-x` | `sc-y` `sc-z` |".
    let mut documented: Vec<(String, Vec<String>)> = Vec::new();
    for line in design.lines() {
        let Some(rest) = line.strip_prefix("| `sc-") else {
            continue;
        };
        let Some((crate_cell, deps_cell)) = rest.split_once("` | ") else {
            continue;
        };
        let name = format!("sc-{crate_cell}");
        let mut deps: Vec<String> = deps_cell
            .trim_end_matches(" |")
            .split_whitespace()
            .filter_map(|tok| tok.strip_prefix('`')?.strip_suffix('`').map(str::to_owned))
            .filter(|tok| tok.starts_with("sc-"))
            .collect();
        deps.sort();
        documented.push((name, deps));
    }
    documented.sort();
    documented.dedup();

    // `sc-test-harness` lives under `tests/`, is a dev-dependency only, and is
    // deliberately absent from both the table and the diagram.
    let documented: Vec<_> = documented
        .into_iter()
        .filter(|(name, _)| name != "sc-test-harness")
        .collect();

    assert_eq!(
        documented, actual,
        "§2's direct-dependency table has drifted from the workspace manifests"
    );

    // Now the arrows. Nodes are declared as `id[\"sc-name\"]`; an edge is
    // `lhs --> rhs`, where either side may carry its declaration.
    let graph = mermaid_block(&design, 0);
    assert!(
        graph.trim_start().starts_with("graph "),
        "the first mermaid block in the design should be the crate graph"
    );
    let mut labels: Vec<(String, String)> = Vec::new();
    for token in graph.split_whitespace() {
        if let Some((id, rest)) = token.split_once("[\"")
            && let Some(name) = rest.strip_suffix("\"]")
        {
            labels.push((id.to_owned(), name.to_owned()));
        }
    }
    let resolve = |token: &str| -> String {
        let id = token.split_once("[\"").map_or(token, |(id, _)| id);
        labels
            .iter()
            .find(|(node, _)| node == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| panic!("the crate diagram uses undeclared node `{id}`"))
    };

    let mut drawn = 0usize;
    for line in graph.lines() {
        let parts: Vec<&str> = line.trim().split(" --> ").collect();
        if parts.len() != 2 {
            continue;
        }
        let (from, to) = (resolve(parts[0]), resolve(parts[1]));
        let deps = actual
            .iter()
            .find(|(name, _)| *name == from)
            .unwrap_or_else(|| panic!("the crate diagram draws `{from}`, which is not a crate"))
            .1
            .clone();
        assert!(
            deps.contains(&to),
            "the crate diagram draws `{from} --> {to}`, but {from} does not depend on {to}"
        );
        drawn += 1;
    }
    assert!(drawn > 20, "the crate diagram lost most of its arrows");

    // Every crate that exists is in the picture.
    for (name, _) in &actual {
        assert!(
            labels.iter().any(|(_, label)| label == name),
            "the crate diagram is missing `{name}`"
        );
    }
}

/// The predictive-models milestone, held to what it built (§14.2): the five
/// nouns, the two seams, the split, the encoding, the job and the storage
/// tables. The section it replaced was a four-method sketch of a trait that was
/// never written that way, which is the failure this test exists to catch —
/// a design document that describes a plan rather than the code.
#[test]
fn the_design_records_what_the_models_milestone_actually_built() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    for fragment in [
        // The five nouns, and where each lives.
        "model provider",
        "model instance",
        "`_fd_models`",
        "`_fd_model_instances`",
        // The dataset, and the vocabulary there deliberately is not.
        "no second vocabulary",
        "neighbourhoodⱵaverage_income",
        // The two seams, and why the crate is at layer 6 rather than above the
        // row layer its data comes from.
        "DatasetSource",
        "ModelProviderHost",
        "`sc-module` (layer 6) can only implement a trait declared *below* it",
        // The split.
        "hash of the primary key",
        "no single primary key",
        // The encoding, and the silent failure it exists to prevent.
        "The encoding belongs to the instance",
        "on the training rows only",
        "not a row of zeros",
        // The outcome as a function of the configuration, and the two halves of
        // a fit result.
        "outcome is a function of the configuration",
        "Metrics are the host's; parameters are the provider's",
        "ParameterBlock",
        // The job, and the two consequences it is honest about.
        "Fitting is a job, not a request",
        "the row is the registry",
        "the server restarted while this fit was running",
        // Once "There is no cancel"; since analytics A3.3 any fit stops at its
        // next stage, and a posterior's processes are killed.
        "A cancel stops a fit between its stages",
        // The grid, the feature, and the third source of providers.
        "validation",
        "`smartcore` feature",
        "feldspar-sklearn",
        // Prediction: a formula and a method (milestone 31), hoisted like a
        // module call and reached through the `ModelHost` seam.
        "Prediction: a formula and a method",
        "hoisted, exactly as a module call is",
        "`ModelHost`",
        "A stored calculated field will not predict",
        "An action is in the set only if it is generic",
        // The storage judgements §9 asks for.
        "the failure **sentence** is in `attributes`",
        "`bytea` column would be the only one",
    ] {
        assert!(
            design.contains(fragment),
            "the design should record `{fragment}`"
        );
    }
    assert!(
        !design.contains("Crates planned in the tree above but **not yet created**: `sc-bus`,\n`sc-fieldview`, `sc-viewpattern`, `sc-model`"),
        "sc-model exists and must not be listed as not yet created"
    );
}

/// Milestone 31 removed `predict_row` and `write_posterior` and the flat
/// `models.draws(name, …)` family. A document that still taught one would send
/// an admin to an "unknown action" — so the documents an admin reads may name
/// the removed actions only to say they are gone.
#[test]
fn no_document_teaches_the_removed_model_actions() {
    let root = workspace_root();
    for doc in [
        "README.md",
        "docs/OPERATIONS.md",
        "docs/TECHNICAL_DESIGN.md",
        "docs/tutorial-models.md",
        "docs/tutorial-stan.md",
        "docs/tutorial-triggers.md",
        "docs/tutorial-python.md",
    ] {
        let text = read(&root, doc);
        for gone in [
            "`predict_row` action",
            "\"action\": \"predict_row\"",
            "\"action\": \"write_posterior\"",
            "models.draws(",
            "models.summary(",
            "models.instance(",
            "\"activate\": true",
        ] {
            assert!(!text.contains(gone), "{doc} still teaches `{gone}`");
        }
    }
}

/// The models tutorial has to reach every screen the milestone's definition of
/// done names, because each one is a place an admin has to be able to find —
/// and a tutorial that quietly lost one would still read fine.
#[test]
fn the_models_tutorial_walks_the_definition_of_done() {
    let root = workspace_root();
    let tutorial = read(&root, "docs/tutorial-models.md");
    for fragment in [
        // The dataset: a field, a join path, an aggregation and the filter.
        "neighbourhoodⱵaverage_income",
        "viewingsↃhouse.length",
        "Filter",
        // The provider, its label picker and the outcome it resolves to.
        "linear_regression",
        "Regression on price",
        // The split, and why it is not a shuffle.
        "hash of its primary key",
        // The fit, and the numbers that make a regression worth reading.
        "std. error",
        "Dropped",
        "Activate",
        // The calculated field that applies it, the stored variant, and the
        // nightly refit that keeps it current.
        "predict(\"House prices\")",
        "estimated_price",
        "update_rows",
        "fit_model",
        "if_clean",
        // The second provider, from a bundled module, and the grid.
        "sklearn_gradient_boosting",
        "Hyperparameter search",
        // …and the two operational facts a reader will meet in production.
        "--model-max-rows",
        "compiled out",
        // Where it all happens since analytics A3: the model editor, its
        // outputs, and the comparison of two models.
        "model editor",
        "More plots",
        "Compare",
    ] {
        assert!(
            tutorial.contains(fragment),
            "the models tutorial should cover `{fragment}`"
        );
    }
}

/// The admin UI's *Predictive models* screens were retired in analytics A3
/// (TODO A3.8): models are made, fitted and read in the Analytics UI's model
/// editor, and the admin's old links redirect there. A document that still
/// sends a reader to the old screens would send them somewhere that is not
/// there, and the screens' source must not creep back into the admin bundle.
#[test]
fn no_document_teaches_the_retired_model_screens() {
    let root = workspace_root();
    for doc in [
        "README.md",
        "docs/OPERATIONS.md",
        "docs/tutorial-models.md",
        "docs/tutorial-stan.md",
        "docs/tutorial-analytics.md",
        "docs/tutorial-python.md",
        "docs/tutorial-triggers.md",
    ] {
        let text = read(&root, doc);
        for gone in [
            "Predictive models →",
            "**Predictive models** is in the sidebar",
            "the Models tab",
            "Models tab says",
            "the instance screen",
            "Edit in Analytics",
        ] {
            assert!(!text.contains(gone), "{doc} still teaches `{gone}`");
        }
    }
    for gone in [
        "ui/admin/src/screens/ModelForm.tsx",
        "ui/admin/src/screens/ModelInstance.tsx",
        "ui/admin/src/screens/PosteriorInstance.tsx",
        "ui/admin/src/models.ts",
    ] {
        assert!(
            !root.join(gone).exists(),
            "{gone} moved to ui/analytics in A3.6 and should not be back"
        );
    }
    let redirect = read(&root, "ui/admin/src/modelRedirect.ts");
    assert!(
        redirect.contains("/analytics/#/models/"),
        "the admin's old model links should lead to the model editor"
    );
}

/// Every `_fd_*` table (and `users`) that some crate bootstraps must appear in
/// §9.2's entity-relationship diagram. A metadata table nobody drew is one an
/// admin discovers in `psql`, which is the failure §9 exists to prevent.
#[test]
fn the_er_diagram_names_every_metadata_table() {
    let root = workspace_root();
    let design = read(&root, "docs/TECHNICAL_DESIGN.md");
    let er = mermaid_block(&design, 1);
    assert!(
        er.trim_start().starts_with("erDiagram"),
        "the second mermaid block in the design should be the ER diagram"
    );

    let mut tables: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut sources);
    for path in &sources {
        let text = fs::read_to_string(path).unwrap_or_default();
        for line in text.lines() {
            // `const SOMETHING_TABLE: &str = "…";` — the `_TABLE` suffix is what
            // separates a table's name from the query-builder's `_fd_`-prefixed
            // column aliases, which are not tables and are not drawn.
            let trimmed = line.trim();
            let Some(rest) = trimmed
                .strip_prefix("pub const ")
                .or_else(|| trimmed.strip_prefix("const "))
                .or_else(|| {
                    trimmed
                        .split_once(") const ")
                        .filter(|(vis, _)| vis.starts_with("pub("))
                        .map(|(_, rest)| rest)
                })
            else {
                continue;
            };
            let Some((name, value)) = rest.split_once(": &str = \"") else {
                continue;
            };
            if !name.ends_with("_TABLE") {
                continue;
            }
            let Some((table, _)) = value.split_once('"') else {
                continue;
            };
            if table.starts_with("_fd_") || table == "users" {
                tables.push(table.to_owned());
            }
        }
    }
    tables.sort();
    tables.dedup();
    assert!(
        tables.len() >= 13,
        "expected the bootstrapped metadata tables, found {tables:?}"
    );
    for table in &tables {
        assert!(
            er.contains(&format!("\"{table}\"")),
            "§9.2's ER diagram does not draw `{table}`"
        );
    }
}

/// The metadata namespace is `_fd_`, and nothing live still spells it `_sc_`.
///
/// Saltcorn v1 keeps its own metadata in `_sc_*` tables, and a transition
/// project runs v1 and this server against **one** schema. So the prefix is not
/// decoration: a table bootstrapped as `_sc_config` here would land on top of
/// v1's, and `is_system` would hide v1's rows from the very admin who came to
/// look at them.
///
/// The query builder's aliases (`_fd_a1`, `_fd_j1`, `_fd_g1`, `_fd_rn`, …) carry
/// the same prefix, and have to: an alias is only guaranteed not to shadow a
/// real table because §9 forbids a user table from starting with the reserved
/// prefix. Two prefixes would mean the reserved one and the alias one could
/// drift apart, and the day a user names a table `_sc_a1` a correlated column
/// reference silently resolves to the wrong row.
///
/// What may still say `_sc_`: a line that says on its own face that it is
/// talking about v1's tables, and the historical records (`docs/TODO-*.md`, the
/// CHANGELOG) that describe work as it was done.
#[test]
fn the_metadata_namespace_is_fd_and_only_v1_is_still_called_sc() {
    let root = workspace_root();

    // The reserved prefix, at the two places that enforce it — the catalog's
    // classifier and the API's create-table guard — read out of the sources
    // rather than restated, because a constant asserted against itself proves
    // nothing.
    let table = read(&root, "crates/sc-catalog/src/table.rs");
    assert!(
        table.contains(r#"self.name.starts_with("_fd_")"#),
        "`Table::is_system` should reserve the `_fd_` prefix"
    );
    let edit = read(&root, "crates/sc-api/src/schema_edit.rs");
    assert!(
        edit.contains(r#"name.starts_with("_fd_")"#),
        "creating a table should refuse the `_fd_` prefix"
    );

    let mut files = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut files);
    collect_rust_sources(&root.join("tests"), &mut files);
    for dir in ["crates", "ui", "docs"] {
        collect_text_sources(&root.join(dir), &mut files);
    }
    files.push(root.join("README.md"));
    assert!(
        files.len() > 100,
        "the sweep should reach the workspace's sources, found {}",
        files.len()
    );

    let mut stale = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        // Historical records describe the work as it was done (see
        // `the_binary_and_everything_it_owns_are_named_feldspar`), and this
        // file is the sweep itself, which has to write the word to look for it.
        // Saltcorn UI's vendored files and browser assets *are* v1 — its
        // `_sc_globalCsrf` and `_sc_lightmode` globals — copied unedited, so
        // every line of them is talking about v1 by construction.
        if rel.contains("TODO")
            || rel.contains("Saltcorn1_description")
            || rel.ends_with("repo_hygiene.rs")
            || rel.starts_with("ui/saltcorn-ui/vendor/")
            || rel.starts_with("ui/saltcorn-ui/public/")
            || rel.starts_with("ui/builder/vendor/")
            || rel.starts_with("ui/builder/public/")
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            // Only where `_sc_` *starts* an identifier. `op_sc_files`,
            // `__sc_db` and `manifest_section_sc_keys` are a Deno op, a Python
            // global and a Rust helper — none of them is a SQL name.
            let starts_identifier = line.match_indices("_sc_").any(|(i, _)| {
                i == 0
                    || !line.as_bytes()[i - 1].is_ascii_alphanumeric()
                        && line.as_bytes()[i - 1] != b'_'
            });
            if starts_identifier && !line.contains("v1") {
                stale.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "`_sc_` is Saltcorn v1's namespace; say so on the line or use `_fd_`:\n{}",
        stale.join("\n")
    );
}

/// Every source file under `dir` whose extension this sweep can read, minus the
/// build outputs and vendored trees nobody in this repository wrote.
fn collect_text_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "node_modules" || n == "dist")
            {
                continue;
            }
            collect_text_sources(&path, out);
        } else if path.extension().is_some_and(|ext| {
            matches!(
                ext.to_string_lossy().as_ref(),
                "ts" | "tsx" | "js" | "py" | "toml" | "md" | "sql"
            )
        }) {
            out.push(path);
        }
    }
}

/// Every `.rs` file under `dir`, recursively.
fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The README's own section cross-references (`§7`, `§10`, …) must name a
/// section it actually has.
///
/// The document is written as numbered sections that point at each other, so
/// inserting one — §2's Debian quick start was inserted ahead of eight existing
/// sections — renumbers every reference after it. A stale `§5` is not a broken
/// link a reader can see through: it sends them to a section about something
/// else. References carrying a sub-section number (`§12.1`) or sitting next to
/// the words "design"/"TECHNICAL_DESIGN" are the *technical design's* sections,
/// not this document's, and are left alone.
#[test]
fn readme_section_references_resolve() {
    let root = workspace_root();
    let readme = read(&root, "README.md");

    let sections: Vec<u32> = readme
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .filter_map(|rest| rest.split_once(". "))
        .filter_map(|(n, _)| n.parse().ok())
        .collect();
    assert!(
        sections.len() >= 11,
        "expected the README's numbered sections, found {sections:?}"
    );

    for (idx, _) in readme.match_indices('§') {
        let rest = &readme[idx + '§'.len_utf8()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue;
        }
        // A sub-section number, or a reference the sentence attributes to the
        // design document: not ours to resolve.
        if rest[digits.len()..].starts_with('.') {
            continue;
        }
        let context = &readme[idx.saturating_sub(60)..idx];
        if context.contains("design") || context.contains("TECHNICAL_DESIGN") {
            continue;
        }
        let number: u32 = digits.parse().expect("digits");
        assert!(
            sections.contains(&number),
            "README references §{number}, which is not one of its sections {sections:?}"
        );
    }
}

/// The `feldspar.toml` the Debian quick start (§2.5) tells an operator to write
/// must parse — with the *real* reader, the one the binary uses.
///
/// The file is `deny_unknown_fields` precisely so that a misspelled key fails
/// instead of quietly connecting somewhere else, which makes a sample in the
/// README a thing that can rot into a startup error on someone's first boot.
#[test]
fn readme_quick_start_config_file_parses() {
    let root = workspace_root();
    let readme = read(&root, "README.md");

    // The sample is the heredoc the quick start pipes into /etc/feldspar.
    let (_, after) = readme
        .split_once("sudo tee /etc/feldspar/feldspar.toml >/dev/null <<'TOML'\n")
        .expect("§2.5 should write the configuration file with a TOML heredoc");
    let sample = after
        .split_once("\nTOML\n")
        .expect("the heredoc should be terminated")
        .0;

    let config = sc_cli::ConfigFile::parse(sample, Path::new("README.md#2.5"))
        .expect("the quick start's feldspar.toml should parse");
    assert_eq!(config.default_environment.as_deref(), Some("production"));

    let production = config
        .environment("production", Path::new("README.md#2.5"))
        .expect("the quick start defines a production environment");
    // Peer authentication over the socket: a host that is a directory, a user,
    // a database — and deliberately no password to leave lying in /etc.
    assert_eq!(production.host.as_deref(), Some("/var/run/postgresql"));
    assert_eq!(production.user.as_deref(), Some("feldspar"));
    assert_eq!(production.database.as_deref(), Some("feldspar"));
    assert!(production.password.is_none() && production.url.is_none());
    // The serving half: without a base domain the server mounts no application.
    assert!(production.base_domain.is_some());
    assert!(production.bind.is_some());
}

/// The quick start's systemd unit has to carry the four lines that make the
/// service work as an unprivileged one, each of which is invisible until it is
/// missing: the state directory it may write to (`ProtectSystem=strict` makes
/// everything else read-only), a home for npm's cache when the server builds an
/// application, and the capability that lets a non-root process bind 80/443.
///
/// `Type=notify` and `WatchdogSec` are asserted too, because they are claims
/// about the *binary*: the server sends `READY=1` once the listener is bound and
/// pings the watchdog while it runs (`sc_server::ServiceManager`). A unit that
/// dropped either would silently give up a guarantee the code still provides;
/// one that kept them against a binary that stopped notifying would hang until
/// systemd's start timeout.
#[test]
fn readme_quick_start_systemd_unit_is_complete() {
    let root = workspace_root();
    let readme = read(&root, "README.md");
    let (_, after) = readme
        .split_once("sudo tee /etc/systemd/system/feldspar.service >/dev/null <<'UNIT'\n")
        .expect("§2.6 should write the unit with a heredoc");
    let unit = after
        .split_once("\nUNIT\n")
        .expect("the heredoc should be terminated")
        .0;

    for line in [
        "Type=notify",
        "WatchdogSec=",
        "User=feldspar",
        "StateDirectory=feldspar",
        "ReadWritePaths=/var/lib/feldspar",
        "Environment=HOME=/var/lib/feldspar",
        "AmbientCapabilities=CAP_NET_BIND_SERVICE",
        "WantedBy=multi-user.target",
    ] {
        assert!(
            unit.contains(line),
            "the quick start's systemd unit should contain `{line}`"
        );
    }
    assert!(
        !unit.contains("Type=simple"),
        "the server notifies readiness, so the unit should claim it rather than Type=simple"
    );
}

/// The command is `feldspar`, and everything the binary owns on disk is named
/// after it.
///
/// The project is **Saltcorn Feldspar**: Saltcorn is the company and the lineage,
/// Feldspar is this rewrite, and the shipped artifact is one binary called
/// `feldspar`. That name reaches an operator through four independent surfaces —
/// the packaged executable, the deployment file it looks for, the environment
/// variables that override it, and the directory it scaffolds into an
/// application — and each of them lives in a different file, so a partial rename
/// is a thing that compiles. Every one is pinned here.
///
/// What deliberately keeps the old name is asserted too, so that a later sweep
/// does not "finish the job" and break something: the Postgres role and database
/// in the quick start are an operator's own objects (and the unit's `User=` must
/// match the role for peer authentication over the socket), and `@saltcorn/…` is
/// the npm scope Saltcorn v1's modules are published under.
#[test]
fn the_binary_and_everything_it_owns_are_named_feldspar() {
    let root = workspace_root();

    // 1. The packaged executable. `[[bin]] name` is what `cargo build` writes,
    //    what `CARGO_BIN_EXE_*` resolves to, and what an operator types.
    let manifest = read(&root, "crates/sc-cli/Cargo.toml");
    let bin = toml_section(&manifest, "[[bin]]").expect("sc-cli declares a [[bin]] target");
    assert!(
        bin.contains(r#"name = "feldspar""#),
        "the CLI binary should be named `feldspar`, not: {bin}"
    );

    // 2. The deployment file and the directory it is searched for in, and 3. the
    //    environment variables that name it — the reader's own constants, not a
    //    doc's spelling of them.
    assert_eq!(sc_cli::config_file::FILE_NAME, "feldspar.toml");
    assert_eq!(sc_cli::config_file::APP_DIR, "feldspar");
    assert_eq!(sc_cli::config_file::CONFIG_PATH_VAR, "FELDSPAR_CONFIG");
    assert_eq!(sc_cli::config_file::ENVIRONMENT_VAR, "FELDSPAR_ENV");

    // 4. The generated runtime the scaffolder writes into an application's
    //    project, which its README, its AGENTS.md and every emitted import agree on.
    assert_eq!(sc_app::REACT_RUNTIME_SUBDIR, "src/feldspar");

    // The install artifact and the unit that runs it.
    let script = read(&root, "scripts/build-static.sh");
    for fragment in ["bin/feldspar", "/opt/feldspar"] {
        assert!(
            script.contains(fragment),
            "scripts/build-static.sh should package `{fragment}`"
        );
    }

    // No live document may still tell a reader to run the old command. The
    // historical records under `docs/TODO-*.md` and the CHANGELOG describe work
    // as it was done and are deliberately left alone.
    for doc in documentation_files(&root) {
        let rel = doc.strip_prefix(&root).unwrap_or(&doc).to_owned();
        let name = rel.to_string_lossy();
        if name.contains("TODO") || name.contains("Saltcorn1_description") {
            continue;
        }
        let text = std::fs::read_to_string(&doc).expect("read a documentation file");
        for stale in [
            "saltcorn serve",
            "saltcorn api ",
            "saltcorn auth ",
            "saltcorn build-app",
            "saltcorn.toml",
            "src/saltcorn",
            "SALTCORN_",
        ] {
            // `SC_SALTCORN_UI_BUNDLE_DIR` names Saltcorn UI's bundle — the
            // framework, not the command — so a `SALTCORN_` the build's own
            // `SC_` prefix introduces is not the old variable namespace.
            let found = if stale == "SALTCORN_" {
                text.match_indices(stale)
                    .any(|(i, _)| !text[..i].ends_with("SC_"))
            } else {
                text.contains(stale)
            };
            assert!(
                !found,
                "{}: the command is `feldspar`, so `{stale}` is stale",
                name
            );
        }
    }

    // The quick start's Postgres role and the unit's service user are both
    // `feldspar` — and, more importantly, are the *same*: §2.3 connects over the
    // Unix socket with peer authentication, which only works when the operating
    // system user and the database role have the same name.
    let readme = read(&root, "README.md");
    assert!(
        readme.contains("CREATE ROLE feldspar") && readme.contains("User=feldspar"),
        "the quick start's Postgres role and the unit's service user should both be \
         `feldspar`, and must match each other for peer authentication over the socket"
    );

    // What keeps the Saltcorn name on purpose, so a later sweep does not take it:
    // `@saltcorn/…` is the npm scope v1's modules are published under, and
    // `globalThis.saltcorn` is the API a v1 module is handed at run time.
    let install = read(&root, "crates/sc-module/src/install.rs");
    assert!(
        install.contains("@saltcorn/"),
        "v1 modules are published under the `@saltcorn/` npm scope, which is not ours to rename"
    );
}

/// Every crate aggregates its integration tests into one binary (`tests/it.rs`,
/// `autotests = false`), because a target per file linked a static V8 180 times
/// and filled CI's disk in the middle of one — `rust-lld` reports that as
/// `signal 7 [Bus error]`, not as "no space left".
///
/// The cost of that is a list to keep in step: a test file nobody adds to
/// `it.rs` is a file cargo no longer builds, and neither the compiler nor a
/// green test run will mention it. So the list is asserted here, both ways — a
/// file that reaches no target, and an entry naming a file that is gone.
#[test]
fn every_integration_test_file_reaches_a_target() {
    let root = workspace_root();
    let mut unreachable = Vec::new();
    let mut dangling = Vec::new();

    let crates = fs::read_dir(root.join("crates")).expect("crates/ should be readable");
    for entry in crates.flatten() {
        let krate = entry.path();
        let tests = krate.join("tests");
        if !tests.is_dir() {
            continue;
        }
        let name = krate
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let mut files: Vec<String> = fs::read_dir(&tests)
            .expect("a tests/ directory should be readable")
            .flatten()
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .filter(|stem| stem != "it")
            .collect();
        files.sort();

        let manifest = fs::read_to_string(krate.join("Cargo.toml")).unwrap_or_default();
        // Cargo still makes a target of every file here, so there is no list to
        // fall out of step with.
        if !manifest.contains("autotests = false") {
            continue;
        }

        // The two ways a file reaches a target: named by a `[[test]]` of its
        // own (for a test that has to keep its own process), or pulled into the
        // aggregate as a module.
        let explicit: Vec<String> = manifest
            .match_indices("[[test]]")
            .filter_map(|(i, _)| toml_section(&manifest[i..], "[[test]]"))
            .filter_map(|section| {
                let line = section
                    .lines()
                    .find(|l| l.trim_start().starts_with("name"))?;
                Some(line.split('"').nth(1)?.to_string())
            })
            .collect();
        let aggregate = fs::read_to_string(tests.join("it.rs")).unwrap_or_default();
        let modules: Vec<String> = aggregate
            .lines()
            .filter_map(|l| l.trim().strip_prefix("#[path = \""))
            .filter_map(|l| l.strip_suffix(".rs\"]"))
            .map(str::to_string)
            .collect();

        for file in &files {
            if !modules.contains(file) && !explicit.contains(file) {
                unreachable.push(format!("{name}/tests/{file}.rs"));
            }
        }
        // Not `files`: an entry may also point at a shared `common/mod.rs`,
        // which is a module of the aggregate rather than a test file.
        for module in &modules {
            if !tests.join(format!("{module}.rs")).is_file() {
                dangling.push(format!("{name}/tests/it.rs names {module}.rs"));
            }
        }
    }

    assert!(
        unreachable.is_empty(),
        "these test files are compiled by nothing — add `#[path]`/`mod` lines for them to \
         their crate's tests/it.rs, or a `[[test]]` if the test needs its own process: {unreachable:#?}"
    );
    assert!(
        dangling.is_empty(),
        "these tests/it.rs entries name a file that is not there: {dangling:#?}"
    );
}
