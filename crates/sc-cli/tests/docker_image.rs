//! `deploy/docker`: the production image, its compose file and its seccomp
//! profile.
//!
//! The image is built from what other files decide — the bucket and object
//! name `scripts/build-static.sh --release` publishes to, the prefix the binary
//! was compiled for, the Node major `feldspar setup` installs — and
//! nothing would notice one of those moving without it. So the fast tests here
//! hold the Dockerfile to them, and hold the seccomp profile to its one
//! deliberate difference from Docker's default.
//!
//! The end-to-end test builds the image and runs it beside a PostgreSQL
//! container. It needs docker, the network, and `docker login dhi.io` (the base
//! image is Docker Hardened Images'), so it is `#[ignore]`d:
//!
//!     cargo test -p sc-cli --test it docker_image -- --ignored

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo().join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

/// The value of a shell assignment `NAME="value"` (or `readonly NAME="value"`,
/// or unquoted) — the first in the file.
fn shell_value(script: &str, name: &str) -> String {
    script
        .lines()
        .map(|line| line.trim().trim_start_matches("readonly "))
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .map(|value| {
            let value = value.split_whitespace().next().unwrap_or_default();
            value.trim_matches('"').trim_end_matches(';').to_owned()
        })
        .unwrap_or_else(|| panic!("no {name}= in the script"))
}

/// The Dockerfile downloads exactly what the release publishes, unpacks it at
/// the prefix the binary was built for, and installs the Node that
/// `feldspar setup` would.
#[test]
fn the_image_installs_the_published_python_artifact_at_its_prefix() {
    let dockerfile = read("deploy/docker/Dockerfile");
    let build = read("scripts/build-static.sh");

    let url = shell_value(&read("scripts/lib/release.sh"), "R2_PUBLIC_BASE");
    assert_eq!(shell_value(&dockerfile, "RELEASE_URL"), url);
    // The x86_64 release's own name, which the build publishes from
    // `feldspar-linux-${TARGET_ARCH}.tar.gz`.
    assert_eq!(
        shell_value(&dockerfile, "RELEASE_OBJECT"),
        "feldspar-linux-x86_64.tar.gz"
    );
    assert!(build.contains(r#"OBJECTS=("feldspar-linux-${TARGET_ARCH}.tar.gz")"#));
    assert_eq!(
        shell_value(&dockerfile, "PREFIX"),
        shell_value(&build, "PREFIX")
    );
    assert!(
        dockerfile.contains(&format!(
            "COPY --from=fetch {0} {0}",
            shell_value(&build, "PREFIX")
        )),
        "the unpacked tree is copied to the same prefix"
    );
    assert_eq!(
        shell_value(&dockerfile, "NODE_MAJOR"),
        sc_cli::setup::NODE_MAJOR.to_string()
    );

    // Both stages on the hardened Debian 13 base, and nothing overridable: the
    // image is one artifact on one base.
    let bases: Vec<&str> = dockerfile
        .lines()
        .filter_map(|l| l.strip_prefix("FROM "))
        .collect();
    assert_eq!(bases.len(), 2, "{bases:?}");
    assert!(
        bases
            .iter()
            .all(|b| b.starts_with("dhi.io/debian-base:trixie-debian13-dev")),
        "{bases:?}"
    );
    assert!(
        !dockerfile.lines().any(|l| l.starts_with("ARG ")),
        "no build arguments"
    );
}

/// The image serves plain HTTP on 8080 as an unprivileged user, with TLS pinned
/// to the proxy, and the compose file runs it the way the image expects.
#[test]
fn the_image_and_its_compose_file_agree_on_how_it_runs() {
    let dockerfile = read("deploy/docker/Dockerfile");
    assert!(dockerfile.contains("FELDSPAR_SSL_MODE=proxy"));
    assert!(dockerfile.contains("USER 10001:10001"));
    assert!(dockerfile.contains("EXPOSE 8080"));
    assert!(dockerfile.contains("http://127.0.0.1:8080/health"));
    assert!(
        dockerfile.contains(
            r#"ENTRYPOINT ["tini", "--", "feldspar", "serve", "--bind", "0.0.0.0:8080"]"#
        )
    );
    // Naming an environment would make a mounted feldspar.toml outrank the
    // PG* variables the image is configured with.
    let instructions = |text: &str| {
        text.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!instructions(&dockerfile).contains("--environment"));

    let compose = read("deploy/docker/compose.yml");
    for line in [
        "- seccomp=./seccomp.json",
        "- no-new-privileges:true",
        "- feldspar-data:/var/lib/feldspar",
        "- \"8080:8080\"",
        "PGHOST: db",
        "read_only: true",
    ] {
        assert!(compose.contains(line), "compose.yml has no `{line}`");
    }
    assert!(!instructions(&compose).contains("--environment"));
    assert!(!instructions(&compose).contains("--no-browser-sandbox"));
}

/// Docker's default profile with one entry added, and that entry allows only
/// what Chromium's sandbox needs, unconditionally.
#[test]
fn the_seccomp_profile_is_docker_s_default_plus_chromium_s_sandbox() {
    let profile: serde_json::Value =
        serde_json::from_str(&read("deploy/docker/seccomp.json")).unwrap();
    assert_eq!(profile["defaultAction"], "SCMP_ACT_ERRNO");
    let rules = profile["syscalls"].as_array().unwrap();
    // The default profile's own shape: a long allow-list, `clone` among it
    // under an argument filter.
    assert!(rules.len() > 40, "{} rules", rules.len());
    let ours = rules.last().unwrap();
    assert_eq!(ours["action"], "SCMP_ACT_ALLOW");
    assert_eq!(
        ours["names"],
        serde_json::json!(["clone", "unshare", "chroot"])
    );
    for condition in ["args", "includes", "excludes"] {
        assert!(ours.get(condition).is_none(), "{ours}");
    }
    assert_eq!(
        rules
            .iter()
            .filter(|r| r["comment"]
                .as_str()
                .is_some_and(|c| c.starts_with("feldspar:")))
            .count(),
        1,
        "exactly one entry is ours"
    );
}

/// `feldspar seccomp-profile` prints the file the image ships, byte for byte.
#[test]
fn feldspar_seccomp_profile_prints_the_shipped_profile() {
    let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .arg("seccomp-profile")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        read("deploy/docker/seccomp.json")
    );
}

/// Containers and a network that are removed however the test ends.
struct Docker {
    containers: Vec<String>,
    network: String,
}

impl Drop for Docker {
    fn drop(&mut self) {
        for name in &self.containers {
            let _ = docker(&["rm", "-f", name]);
        }
        let _ = docker(&["network", "rm", &self.network]);
    }
}

fn docker(args: &[&str]) -> (bool, String) {
    let out = Command::new("docker").args(args).output().unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The real thing: the image builds, boots against PostgreSQL with TLS pinned
/// to the proxy, answers its health check, and starts Chromium sandboxed under
/// the shipped profile — while under Docker's default profile the boot says
/// `view_app` is unavailable and names the fix.
#[test]
#[ignore = "builds the Docker image: needs docker, the network and `docker login dhi.io`"]
fn the_image_serves_and_keeps_chromium_sandboxed() {
    let tag = "feldspar-image-test";
    let context = repo().join("deploy/docker");
    let (ok, out) = docker(&["build", "-t", tag, context.to_str().unwrap()]);
    assert!(ok, "{out}");

    let id = std::process::id();
    let network = format!("feldspar-test-{id}");
    let db = format!("feldspar-test-db-{id}");
    let app = format!("feldspar-test-app-{id}");
    let bare = format!("feldspar-test-bare-{id}");
    let _cleanup = Docker {
        containers: vec![app.clone(), bare.clone(), db.clone()],
        network: network.clone(),
    };
    assert!(docker(&["network", "create", &network]).0);
    let (ok, out) = docker(&[
        "run",
        "-d",
        "--name",
        &db,
        "--network",
        &network,
        "--network-alias",
        "db",
        "-e",
        "POSTGRES_USER=feldspar",
        "-e",
        "POSTGRES_PASSWORD=pw",
        "-e",
        "POSTGRES_DB=feldspar",
        "postgres:17",
    ]);
    assert!(ok, "{out}");
    let ready = (0..60).any(|_| {
        std::thread::sleep(std::time::Duration::from_secs(1));
        docker(&[
            "exec",
            &db,
            "pg_isready",
            "-U",
            "feldspar",
            "-d",
            "feldspar",
        ])
        .0
    });
    assert!(ready, "postgres did not come up");

    let profile = format!("seccomp={}", context.join("seccomp.json").display());
    let run = |name: &str, seccomp: Option<&str>| {
        let mut args = vec!["run", "-d", "--name", name, "--network", &network];
        if let Some(profile) = seccomp {
            args.extend(["--security-opt", profile]);
        }
        args.extend([
            "--security-opt",
            "no-new-privileges:true",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--shm-size",
            "512m",
            "-e",
            "PGHOST=db",
            "-e",
            "PGUSER=feldspar",
            "-e",
            "PGPASSWORD=pw",
            "-e",
            "PGDATABASE=feldspar",
            tag,
            "--base-domain",
            "example.test",
        ]);
        let (ok, out) = docker(&args);
        assert!(ok, "{out}");
    };
    let healthy = |name: &str| {
        (0..90).any(|_| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            docker(&["exec", name, "curl", "-fsS", "http://127.0.0.1:8080/health"]).0
        })
    };

    run(&app, Some(&profile));
    assert!(healthy(&app), "{}", docker(&["logs", &app]).1);
    let logs = docker(&["logs", &app]).1;
    assert!(
        logs.contains("ssl_mode set by the host configuration"),
        "{logs}"
    );
    assert!(logs.contains("view_app will use the browser"), "{logs}");
    let (ok, page) = docker(&[
        "exec",
        &app,
        "chromium",
        "--headless",
        "--disable-gpu",
        "--user-data-dir=/tmp/sandbox-test",
        "--dump-dom",
        "data:text/html,<p>SANDBOXED</p>",
    ]);
    assert!(ok && page.contains("<p>SANDBOXED</p>"), "{page}");

    // The same image under Docker's default profile: it serves, and says why
    // view_app is off and what to change, rather than dropping the sandbox.
    let _ = docker(&["rm", "-f", &app]);
    run(&bare, None);
    assert!(healthy(&bare), "{}", docker(&["logs", &bare]).1);
    let logs = docker(&["logs", &bare]).1;
    assert!(logs.contains("view_app is unavailable"), "{logs}");
    assert!(logs.contains("feldspar seccomp-profile"), "{logs}");
}
