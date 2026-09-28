//! `stanc` against a **fake** `stanc` (TODO 2.4): a shell script that records
//! how it was called and answers what a test tells it to, so the mapping of
//! paths, the handling of a refusal and the comparison with our parse are
//! exercised without a CmdStan. The same checks against the real `stanc` are
//! in `real_cmdstan.rs`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sc_stan::program::{Program, ProgramFile};
use sc_stan::stanc::{check_program, run_stanc};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-stan-fake-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `stanc` that writes its arguments, its working directory and the files
/// it was given into `log`, prints `stdout` and `stderr`, and exits `code`.
fn fake_stanc(dir: &Path, stdout: &str, stderr: &str, code: i32) -> PathBuf {
    let path = dir.join("stanc");
    let log = dir.join("log");
    std::fs::write(dir.join("stdout"), stdout).unwrap();
    std::fs::write(dir.join("stderr"), stderr).unwrap();
    let script = format!(
        "#!/bin/sh\n\
         echo \"args: $*\" > '{log}'\n\
         echo \"cwd: $(pwd)\" >> '{log}'\n\
         echo \"env: $(env | cut -d= -f1 | sort | tr '\\n' ' ')\" >> '{log}'\n\
         find . -type f | sort >> '{log}'\n\
         cat models/lib.stan >> '{log}' 2>/dev/null\n\
         cat '{dir}/stdout'\n\
         sed \"s|WORK|$(pwd)|g\" '{dir}/stderr' >&2\n\
         exit {code}\n",
        log = log.display(),
        dir = dir.display(),
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn program() -> Program {
    Program::from_files(
        "store",
        vec![
            ProgramFile::new(
                "models/m.stan",
                "functions {\n#include lib.stan\n}\ndata { int N; vector[N] y; }\n\
                 parameters { real mu; }\n",
            ),
            ProgramFile::new("models/lib.stan", "real f(real x) { return x; }\n"),
        ],
    )
    .unwrap()
}

const INFO: &str = r#"{
  "inputs": { "N": { "type": "int", "dimensions": 0 }, "y": { "type": "real", "dimensions": 1 } },
  "parameters": { "mu": { "type": "real", "dimensions": 0 } },
  "transformed parameters": {},
  "generated quantities": {},
  "functions": [], "distributions": [], "included_files": [ "./models/lib.stan" ]
}"#;

#[tokio::test]
async fn stanc_runs_over_the_program_laid_out_at_its_store_paths() {
    let dir = scratch("layout");
    let stanc = fake_stanc(
        &dir,
        INFO,
        "Warning in 'WORK/models/m.stan', line 4: something mild",
        0,
    );
    let checked = check_program(&stanc, &program(), &dir.join("work"))
        .await
        .unwrap();
    assert_eq!(checked.interface, program().interface().unwrap());
    // The scratch directory is taken out of the path: what is left is the
    // store path.
    assert_eq!(
        checked.warnings,
        "Warning in 'models/m.stan', line 4: something mild"
    );
    assert_eq!(checked.notice, None);

    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(
        log.contains("args: --info --include-paths=. models/m.stan"),
        "{log}"
    );
    assert!(
        log.contains("./models/lib.stan\n./models/m.stan\n"),
        "{log}"
    );
    // The environment is scrubbed (PWD and friends are the shell's own).
    let env = log.lines().find(|l| l.starts_with("env: ")).unwrap();
    assert!(!env.contains("CARGO"), "{env}");
    // The scratch directory is gone afterwards.
    assert_eq!(std::fs::read_dir(dir.join("work")).unwrap().count(), 0);
}

#[tokio::test]
async fn an_include_is_rewritten_to_the_path_stanc_will_find() {
    let dir = scratch("rewrite");
    let program = Program::from_files(
        "store",
        vec![
            ProgramFile::new("models/m.stan", "functions {\n#include lib.stan\n}\n"),
            ProgramFile::new("models/lib.stan", "#include \"sub/../g.stan\"\n"),
            ProgramFile::new("models/g.stan", "real g(real x) { return x; }\n"),
        ],
    )
    .unwrap();
    let stanc = fake_stanc(&dir, "{}", "", 0);
    run_stanc(&stanc, &program, &dir.join("work"))
        .await
        .unwrap();
    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(log.contains("#include \"models/g.stan\""), "{log}");
}

#[tokio::test]
async fn a_refusal_is_stancs_own_words_with_store_paths() {
    let dir = scratch("refused");
    let stanc = fake_stanc(
        &dir,
        "",
        "Syntax error in './models/lib.stan', line 1, column 29, included from\n\
         'models/m.stan', line 2, column 0, parsing error:\n\
         Ill-formed statement.",
        1,
    );
    let err = check_program(&stanc, &program(), &dir.join("work"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "stanc refused the program `models/m.stan`:\nSyntax error in 'models/lib.stan', line \
             1, column 29, included from\n'models/m.stan', line 2"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_disagreement_with_stanc_is_a_bug_report() {
    let dir = scratch("disagree");
    let info = INFO.replace(
        r#""y": { "type": "real", "dimensions": 1 }"#,
        r#""y": { "type": "real", "dimensions": 2 }"#,
    );
    let stanc = fake_stanc(&dir, &info, "", 0);
    let err = check_program(&stanc, &program(), &dir.join("work"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("which is a bug in Feldspar's Stan parser"),
        "{err}"
    );
    assert!(
        err.contains(
            "`y` in the `data` block: we read real with 1 dimension(s), stanc reads real with 2"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn output_that_is_not_json_is_said_to_be_so() {
    let dir = scratch("garbage");
    let stanc = fake_stanc(&dir, "hello", "", 0);
    let err = run_stanc(&stanc, &program(), &dir.join("work"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("its --info output is not JSON"), "{err}");
}
