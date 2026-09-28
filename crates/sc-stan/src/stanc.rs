//! `stanc`, the authority on whether a program is valid (TODO §5).
//!
//! Our own [parser](crate::program) reads only what the binder needs; it does
//! not type-check a model block and should not try. So when CmdStan is
//! available, checking a program runs `stanc --info` — which parses and
//! type-checks, generates no C++, and takes about a second — and:
//!
//! - its **diagnostics are shown verbatim**, with every path in them mapped
//!   back to the store path the admin knows (the program is laid out under a
//!   scratch directory at its store paths, and `stanc` runs there, so the
//!   mapping is mostly `stanc`'s own relative paths; the rest is stripping the
//!   scratch directory and `./`);
//! - its **`--info` is compared with our parse**: the names, base types and
//!   number of dimensions of every variable of every block we read. A
//!   disagreement is a bug in our parser, and the sentence says so rather than
//!   silently preferring either reading.
//!
//! The child's environment is scrubbed to `PATH`, `HOME` and `TMPDIR` (TODO
//! §13), it is killed when dropped, and it is given [`STANC_TIMEOUT`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sc_error::{Context, Error, Result};
use sc_model::{Declaration, Element, Interface};
use serde_json::Value as Json;

use crate::program::Program;

/// How long `stanc` may take over one program. It takes about a second; this
/// is a bound on a hang, not on a program.
pub const STANC_TIMEOUT: Duration = Duration::from_secs(60);

/// What one run of `stanc --info` said.
#[derive(Debug, Clone, PartialEq)]
pub struct StancOutput {
    /// Whether it accepted the program.
    pub ok: bool,
    /// Its warnings or errors, verbatim but for the paths, which are store
    /// paths. Empty when it had nothing to say.
    pub diagnostics: String,
    /// The `--info` JSON, when it accepted the program.
    pub info: Option<Json>,
}

/// A program `stanc` accepted and our parse agrees with.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramCheck {
    /// What it declares.
    pub interface: Interface,
    /// `stanc`'s warnings, if it had any — shown, never fatal.
    pub warnings: String,
    /// Why the program was *not* checked by `stanc`, when it was not: there
    /// is no CmdStan, so our parse is all there is (§5).
    pub notice: Option<String>,
}

/// Run `stanc --info` (the executable at `stanc`) over `program`, laid out in
/// a fresh directory under `scratch`, which is removed afterwards.
pub async fn run_stanc(stanc: &Path, program: &Program, scratch: &Path) -> Result<StancOutput> {
    let work = ScratchDir::new(scratch)?;
    let main = program.layout(&work.0)?;
    let mut command = tokio::process::Command::new(stanc);
    command
        .arg("--info")
        .arg("--include-paths=.")
        .arg(&main)
        .current_dir(&work.0)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    let output = tokio::time::timeout(STANC_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            Error::msg(format!(
                "stanc did not finish checking `{main}` within {} seconds",
                STANC_TIMEOUT.as_secs()
            ))
        })?
        .with_context(|| format!("running {}", stanc.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut diagnostics = map_paths(stderr.trim_end(), &work.0);
    if !output.status.success() {
        // `stanc` writes its errors to stderr; anything it wrote to stdout on
        // the way out is part of the story too.
        if diagnostics.is_empty() {
            diagnostics = map_paths(stdout.trim_end(), &work.0);
        }
        if diagnostics.is_empty() {
            diagnostics = format!("stanc exited with {} and said nothing", output.status);
        }
        return Ok(StancOutput {
            ok: false,
            diagnostics,
            info: None,
        });
    }
    let info: Json = serde_json::from_str(&stdout).map_err(|e| {
        Error::msg(format!(
            "stanc accepted `{main}` but its --info output is not JSON ({e}): {}",
            stdout.chars().take(200).collect::<String>()
        ))
    })?;
    Ok(StancOutput {
        ok: true,
        diagnostics,
        info: Some(info),
    })
}

/// Check `program` with `stanc` and our parser: refused with `stanc`'s own
/// words when it refuses it, with our sentence when we cannot read it or bind
/// it, and as a bug report when the two readings disagree.
pub async fn check_program(
    stanc: &Path,
    program: &Program,
    scratch: &Path,
) -> Result<ProgramCheck> {
    let output = run_stanc(stanc, program, scratch).await?;
    if !output.ok {
        return Err(Error::invalid(format!(
            "stanc refused the program `{}`:\n{}",
            program.main_path(),
            output.diagnostics
        )));
    }
    let interface = program.interface()?;
    if let Some(info) = &output.info {
        let disagreements = compare(&interface, info);
        if !disagreements.is_empty() {
            return Err(Error::msg(format!(
                "Feldspar's reading of `{}` disagrees with stanc's, which is a bug in Feldspar's \
                 Stan parser — please report it with the program: {}",
                program.main_path(),
                disagreements.join("; ")
            )));
        }
    }
    Ok(ProgramCheck {
        interface,
        warnings: output.diagnostics,
        notice: None,
    })
}

/// Where our reading of each block differs from `stanc --info`'s, one
/// sentence per variable. Empty when they agree.
pub fn compare(ours: &Interface, info: &Json) -> Vec<String> {
    let sections: [(&str, &[Declaration]); 4] = [
        ("inputs", &ours.data),
        ("parameters", &ours.parameters),
        ("transformed parameters", &ours.transformed),
        ("generated quantities", &ours.generated),
    ];
    let mut out = Vec::new();
    for (key, declared) in sections {
        let block = if key == "inputs" { "data" } else { key };
        let theirs: BTreeMap<&str, (String, u64)> = info
            .get(key)
            .and_then(Json::as_object)
            .map(|vars| {
                vars.iter()
                    .map(|(name, v)| (name.as_str(), stanc_shape(v)))
                    .collect()
            })
            .unwrap_or_default();
        let mine: BTreeMap<&str, (String, u64)> = declared
            .iter()
            .map(|d| {
                (
                    d.name.as_str(),
                    (element_name(d.element).to_owned(), d.rank() as u64),
                )
            })
            .collect();
        for (name, shape) in &mine {
            match theirs.get(name) {
                None => out.push(format!(
                    "we read `{name}` in the `{block}` block and stanc does not"
                )),
                Some(other) if other != shape => out.push(format!(
                    "`{name}` in the `{block}` block: we read {} with {} dimension(s), stanc \
                     reads {} with {}",
                    shape.0, shape.1, other.0, other.1
                )),
                Some(_) => {}
            }
        }
        for name in theirs.keys().filter(|n| !mine.contains_key(*n)) {
            out.push(format!(
                "stanc reads `{name}` in the `{block}` block and we do not"
            ));
        }
    }
    out
}

/// `{"type": "real", "dimensions": 2}` as a (base type, dimensions) pair; a
/// tuple's `type` is the list of its parts.
fn stanc_shape(v: &Json) -> (String, u64) {
    let ty = match v.get("type") {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Array(_)) => "tuple".to_owned(),
        _ => "unknown".to_owned(),
    };
    let dims = v.get("dimensions").and_then(Json::as_u64).unwrap_or(0);
    (ty, dims)
}

fn element_name(element: Element) -> &'static str {
    match element {
        Element::Int => "int",
        Element::Real => "real",
        Element::Complex => "complex",
        Element::Tuple => "tuple",
    }
}

/// `stanc`'s text with the scratch directory taken out of every path in it,
/// so what is left is the store path.
fn map_paths(text: &str, work: &Path) -> String {
    let mut text = text.to_owned();
    for root in [work.to_path_buf(), work.canonicalize().unwrap_or_default()] {
        let root = root.display().to_string();
        if !root.is_empty() {
            text = text.replace(&format!("{root}/"), "");
        }
    }
    text.replace("'./", "'").replace("\"./", "\"")
}

/// A directory of its own under the scratch root, removed when dropped.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(root: &Path) -> Result<ScratchDir> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = root.join(format!(
            "stanc-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(ScratchDir(dir))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::SizeExpr;
    use serde_json::json;

    #[test]
    fn paths_in_diagnostics_become_store_paths() {
        let work = Path::new("/tmp/stanc-1-2-3");
        let text = "Syntax error in './lib/g.stan', line 1, included from\n\
                    'models/m.stan', line 2, and /tmp/stanc-1-2-3/models/m.stan";
        assert_eq!(
            map_paths(text, work),
            "Syntax error in 'lib/g.stan', line 1, included from\n'models/m.stan', line 2, and \
             models/m.stan"
        );
    }

    #[test]
    fn our_reading_and_stancs_are_compared_variable_by_variable() {
        let ours = Interface {
            data: vec![
                Declaration::new("N", Element::Int, vec![], "int"),
                Declaration::new(
                    "y",
                    Element::Real,
                    vec![SizeExpr::var("N"), SizeExpr::literal(2)],
                    "array[N] vector[2]",
                ),
            ],
            parameters: vec![Declaration::new("mu", Element::Real, vec![], "real")],
            transformed: vec![],
            generated: vec![Declaration::new(
                "t",
                Element::Tuple,
                vec![],
                "tuple(real, int)",
            )],
        };
        let agree = json!({
            "inputs": {"N": {"type": "int", "dimensions": 0}, "y": {"type": "real", "dimensions": 2}},
            "parameters": {"mu": {"type": "real", "dimensions": 0}},
            "transformed parameters": {},
            "generated quantities": {"t": {"type": [{"type": "real", "dimensions": 0}], "dimensions": 0}},
        });
        assert!(compare(&ours, &agree).is_empty());

        let disagree = json!({
            "inputs": {"N": {"type": "int", "dimensions": 0}, "y": {"type": "real", "dimensions": 1}},
            "parameters": {"mu": {"type": "real", "dimensions": 0}, "sigma": {"type": "real", "dimensions": 0}},
            "generated quantities": {},
        });
        assert_eq!(
            compare(&ours, &disagree),
            [
                "`y` in the `data` block: we read real with 2 dimension(s), stanc reads real with 1",
                "stanc reads `sigma` in the `parameters` block and we do not",
                "we read `t` in the `generated quantities` block and stanc does not",
            ]
        );
    }
}
