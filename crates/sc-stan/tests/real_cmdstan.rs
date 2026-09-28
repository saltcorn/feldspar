//! Against a real CmdStan: `#[ignore]`d, because building one takes a while.
//!
//! The CmdStan is the one `discover` finds from this process's environment —
//! `$CMDSTAN`, else the newest built `~/.cmdstan/cmdstan-*` — so on a machine
//! where `feldspar cmdstan install` has been run these need nothing set:
//!
//! ```text
//! cargo test -p sc-stan -- --ignored
//! CMDSTAN=/opt/cmdstan-2.40.0 cargo test -p sc-stan -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_stan::cmdstan::{CmdStan, Locations, Toolchain, discover};

/// The CmdStan on this machine, ready to compile with; a failure says why not.
fn cmdstan() -> CmdStan {
    let locations = Locations::from_env(None);
    let found = discover(&locations).unwrap_or_else(|e| {
        panic!("{e}\n(run `feldspar cmdstan install`, or set $CMDSTAN, to run these tests)")
    });
    assert!(found.built(), "{} is not built", found.dir.display());
    let tools = Toolchain::find(&locations);
    assert!(tools.ready(), "{:?}", tools.missing());
    found
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-stan-real-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The draws of `column` in a CmdStan output CSV (comment lines skipped).
fn column(csv: &Path, name: &str) -> Vec<f64> {
    let text = std::fs::read_to_string(csv).unwrap();
    let mut rows = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty());
    let header: Vec<&str> = rows.next().unwrap().split(',').collect();
    let at = header.iter().position(|h| *h == name).unwrap();
    rows.map(|row| row.split(',').nth(at).unwrap().parse().unwrap())
        .collect()
}

/// The `bernoulli` example that ships with CmdStan, compiled outside the
/// CmdStan tree (as the compile cache will, TODO §13) and sampled: 2 successes
/// in 10 trials under a uniform prior is Beta(3, 9), whose mean is 0.25.
#[test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
fn the_bernoulli_example_compiles_and_samples() {
    let cmdstan = cmdstan();
    let work = scratch("bernoulli");
    let example = cmdstan.dir.join("examples").join("bernoulli");
    for file in ["bernoulli.stan", "bernoulli.data.json"] {
        std::fs::copy(example.join(file), work.join(file)).unwrap();
    }

    let exe = work.join(format!("bernoulli{}", std::env::consts::EXE_SUFFIX));
    run(Command::new("make").arg(&exe).current_dir(&cmdstan.dir));
    assert!(exe.is_file());

    let output = work.join("output.csv");
    let stdout = run(Command::new(&exe)
        .arg("sample")
        .arg("num_samples=1000")
        .arg("random")
        .arg("seed=4711")
        .arg("data")
        .arg(format!(
            "file={}",
            work.join("bernoulli.data.json").display()
        ))
        .arg("output")
        .arg(format!("file={}", output.display()))
        .current_dir(&work));
    // The progress lines TODO §13's runner will parse.
    assert!(
        stdout.contains("Iteration: 2000 / 2000 [100%]  (Sampling)"),
        "{stdout}"
    );

    let theta = column(&output, "theta");
    assert_eq!(theta.len(), 1000);
    let mean = theta.iter().sum::<f64>() / theta.len() as f64;
    assert!(
        (mean - 0.25).abs() < 0.03,
        "posterior mean of theta was {mean}"
    );
    std::fs::remove_dir_all(&work).unwrap();
}
