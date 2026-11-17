//! Prediction for new rows from a fitted posterior (TODO §19, analytics TODO
//! A7.1): CmdStan's **standalone generated quantities**.
//!
//! `method=generate_quantities fitted_params=chain-1.csv` runs only the
//! `generated quantities` block, over the draws a fit already made, with new
//! data — no refit. CmdStan takes the draws only as its own CSVs and the data
//! only as a file, so a model that predicts keeps its raw run in a file store
//! (`runs_store`, checked on save). A prediction reads that run back, puts
//! the host's binding of the new rows in place of the empty `new` the fit was
//! bound with, compiles the program from the fit's snapshot (a cache hit, as a
//! rule), runs one process per chain, and answers each new row's draws of the
//! prediction.
//!
//! It is a subprocess per chain per call — a second or two — so a `predict()`
//! on a busy trigger is the wrong tool; `predictRows` over a filter is one call
//! for all of its rows.

use std::time::Duration;

use sc_error::{Context, Error, Result};
use sc_model::PosteriorPrediction;
use serde_json::Value as Json;

use crate::compile::{map_src, stop_error};
use crate::output::read_draws;
use crate::process::{self, Tail, Watch};
use crate::program::{Program, ProgramFile};
use crate::run::{DATA_FILE, Method, RunSettings};
use crate::run_dir::{RunDir, RunLocation, read_published};
use crate::{STAN_PROVIDER, StanProvider};

/// How long one prediction may run, compile included.
const PREDICTION_LIMIT: Duration = Duration::from_secs(10 * 60);

impl StanProvider {
    /// The draws of `request.variable` for each new row, from the fit whose
    /// state is `state` (see the module docs).
    pub(crate) async fn generate_quantities(
        &self,
        state: &Json,
        request: &PosteriorPrediction,
    ) -> Result<Vec<Vec<f64>>> {
        let Some(cmdstan) = self.cmdstan() else {
            return Err(Error::config(format!(
                "this Stan model cannot predict because CmdStan was not found: {}",
                self.unavailable().unwrap_or("it is not installed")
            )));
        };
        let run: RunLocation = match state.get("run").filter(|r| !r.is_null()) {
            Some(run) => serde_json::from_value(run.clone()).map_err(|e| {
                Error::msg(format!("the instance's run location is unreadable: {e}"))
            })?,
            None => {
                return Err(Error::invalid(
                    "this fit kept no raw run, and a prediction from a posterior runs the \
                     program's generated quantities over the fit's own draws: choose a runs \
                     store for the model and fit it again",
                ));
            }
        };
        let settings: RunSettings =
            serde_json::from_value(state.get("settings").cloned().unwrap_or(Json::Null))
                .map_err(|e| Error::msg(format!("the fit's recorded settings: {e}")))?;
        let program = snapshot(state)?;

        let store = self.stores().store(&run.store)?;
        let files = read_published(store.as_ref(), &run.path)
            .await
            .with_context(|| {
                format!(
                    "the raw run {} in the file store `{}` could not be read",
                    run.path, run.store
                )
            })?;
        let file = |name: &str| files.iter().find(|(p, _)| p == name).map(|(_, b)| b);
        let mut data: serde_json::Map<String, Json> = match file(DATA_FILE) {
            Some(bytes) => serde_json::from_slice(bytes)
                .map_err(|e| Error::msg(format!("the fit's {DATA_FILE}: {e}")))?,
            None => {
                return Err(Error::not_found(format!(
                    "the raw run {} in the file store `{}` has no {DATA_FILE}",
                    run.path, run.store
                )));
            }
        };
        data.extend(request.data.clone());

        let never = || false;
        let watch = Watch::new(PREDICTION_LIMIT, &never);
        let compiled = self
            .compile_cache()
            .compile(cmdstan, &program, &watch, &|| {})
            .await?;

        let dir = RunDir::create(self.scratch(), &format!("predict-{}", uuid::Uuid::new_v4()))?;
        dir.write(
            DATA_FILE,
            &serde_json::to_vec(&Json::Object(data))
                .map_err(|e| Error::msg(format!("writing JSON: {e}")))?,
        )?;
        let warmup_rows = match settings.method {
            Method::Sample if settings.save_warmup => {
                usize::try_from(settings.iter_warmup.div_ceil(settings.thin)).unwrap_or(0)
            }
            _ => 0,
        };

        let mut out = vec![Vec::new(); request.rows];
        for chain in 1..=settings.processes() {
            let fitted = RunSettings::csv_name(chain);
            let Some(bytes) = file(&fitted) else {
                return Err(Error::not_found(format!(
                    "the raw run {} in the file store `{}` has no {fitted}",
                    run.path, run.store
                )));
            };
            dir.write(&fitted, bytes)?;
            let output = format!("gq-{chain}.csv");
            let mut command = process::command(&compiled.exe);
            for arg in [
                "method=generate_quantities".to_owned(),
                format!("fitted_params={fitted}"),
                "data".to_owned(),
                format!("file={DATA_FILE}"),
                "output".to_owned(),
                format!("file={output}"),
                format!("sig_figs={}", settings.sig_figs),
            ] {
                command.positional(sc_proc::Operand::new(arg)?);
            }
            command.cwd(dir.path());
            let mut child = command.spawn().with_context(|| {
                format!("running the compiled program {}", compiled.exe.display())
            })?;
            let mut lines = process::output_lines(&mut child);
            let mut tail = Tail::default();
            loop {
                tokio::select! {
                    line = lines.recv() => match line {
                        Some(line) => tail.push(&line),
                        None => break,
                    },
                    why = watch.until_stopped() => {
                        child.kill_group();
                        return Err(stop_error(why, &watch));
                    }
                }
            }
            let status = child
                .wait()
                .await
                .context("waiting for generated quantities")?;
            let path = dir.path().join(&output);
            if !status.success() || !path.is_file() {
                return Err(Error::msg(format!(
                    "the generated quantities of `{}` for chain {chain} exited with {status}; \
                     the last lines of its output:\n{}",
                    program.main_path(),
                    map_src(&tail.text(), &compiled.src)
                )));
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let draws = read_draws(&text, chain, warmup_rows)?;
            for series in draws
                .into_iter()
                .filter(|s| s.variable == request.variable && !s.warmup)
            {
                let Some(&i) = series.element.first() else {
                    continue;
                };
                if let Some(row) = i.checked_sub(1).and_then(|i| out.get_mut(i)) {
                    row.extend(series.draws);
                }
            }
        }
        if let Some(i) = out.iter().position(Vec::is_empty) {
            return Err(Error::invalid(format!(
                "the generated quantities wrote no draws of `{}[{}]`: check that the program \
                 computes it for every row of `new`",
                request.variable,
                i + 1
            )));
        }
        Ok(out)
    }
}

/// The program a fit snapshotted (§6), as it was fitted.
fn snapshot(state: &Json) -> Result<Program> {
    let unreadable = || {
        Error::msg(format!(
            "the fit's state has no program snapshot, so the `{STAN_PROVIDER}` provider cannot \
             run its generated quantities; fit it again"
        ))
    };
    let program = state.get("program").ok_or_else(unreadable)?;
    let store = program
        .get("store")
        .and_then(Json::as_str)
        .ok_or_else(unreadable)?;
    let files = program
        .get("files")
        .and_then(Json::as_array)
        .ok_or_else(unreadable)?
        .iter()
        .map(|f| {
            Some(ProgramFile::new(
                f.get("path")?.as_str()?,
                f.get("text")?.as_str()?,
            ))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unreadable)?;
    Program::from_files(store, files)
}
