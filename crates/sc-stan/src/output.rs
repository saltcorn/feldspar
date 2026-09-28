//! CmdStan's output CSV, read into draws (TODO §14).
//!
//! The file is comment lines (`#` — the configuration, the adaptation, the
//! timing), one header line, and one line per saved iteration. What matters
//! here:
//!
//! - **element indices come from the column names** — `Sigma.2.1` is
//!   `Sigma[2, 1]` — never from column position, because CmdStan writes
//!   matrices column-major and a reader that assumed otherwise would transpose
//!   every covariance matrix without a word;
//! - `lp__` and the sampler's columns (`accept_stat__`, `divergent__`, …) are
//!   variables like any other, so the diagnostics can be recomputed from the
//!   stored draws alone;
//! - with `save_warmup`, the first `⌈iter_warmup / thin⌉` rows are warmup;
//! - the comments after the header are the **adaptation** (the step size and
//!   the inverse metric NUTS settled on) and, at the end, the **timing**
//!   (`Elapsed Time: 0.002 seconds (Warm-up)` and the lines under it) —
//!   recorded on the instance, because "chain 3 took four times as long" is a
//!   diagnostic too.
//!
//! The file is read line by line: a program that saves a 50 000-element
//! `y_rep` writes a CSV of hundreds of megabytes, and the columns of a variable
//! the host will not read ([`sc_model::PosteriorInput::unread`]) are skipped
//! as they go past rather than held.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use sc_error::{Context, Error, Result};
use sc_model::DrawSeries;

/// `alpha.3` as (`alpha`, `[3]`); `lp__` as (`lp__`, `[]`).
pub fn parse_column(name: &str) -> Result<(String, Vec<usize>)> {
    let mut parts = name.split('.');
    let variable = parts.next().unwrap_or_default().to_owned();
    let element = parts
        .map(|p| {
            p.parse::<usize>().map_err(|_| {
                Error::msg(format!(
                    "CmdStan's output has a column `{name}` whose index `{p}` is not a number"
                ))
            })
        })
        .collect::<Result<Vec<usize>>>()?;
    Ok((variable, element))
}

/// What NUTS adapted to during warmup.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Adaptation {
    /// The step size.
    pub step_size: f64,
    /// The inverse metric: one row for a diagonal metric (the default), one
    /// row per parameter for a dense one. Empty when the file does not say.
    pub inverse_metric: Vec<Vec<f64>>,
}

/// Everything read out of one CmdStan CSV.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainOutput {
    /// One series per column (two with warmup saved), skipped columns aside.
    pub draws: Vec<DrawSeries>,
    /// What warmup adapted to, for `sample`.
    pub adaptation: Option<Adaptation>,
    /// CmdStan's own timing, by its label (`Warm-up`, `Sampling`, `Total`;
    /// `Pathfinders`, `PSIS`), in seconds.
    pub timing: BTreeMap<String, f64>,
}

/// Where in the comments the reader is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Other,
    /// After `Adaptation terminated`.
    Adaptation,
    /// After `Diagonal elements of inverse mass matrix:` (one line) or
    /// `Elements of inverse mass matrix:` (a line per row).
    Metric {
        dense: bool,
    },
    /// After `Elapsed Time:`.
    Timing,
}

/// A header column: its variable and element, or `None` when it is skipped.
type Column = Option<(String, Vec<usize>)>;

/// A CSV read a line at a time.
struct Reader<'a> {
    chain: u32,
    warmup_rows: usize,
    skip: &'a dyn Fn(&str) -> bool,
    /// The header's columns, once it has been read.
    columns: Option<Vec<Column>>,
    values: Vec<Vec<f64>>,
    rows: usize,
    section: Section,
    step_size: Option<f64>,
    metric: Vec<Vec<f64>>,
    timing: BTreeMap<String, f64>,
}

impl Reader<'_> {
    fn line(&mut self, line: &str) -> Result<()> {
        let line = line.trim_end();
        if line.is_empty() {
            return Ok(());
        }
        if let Some(comment) = line.strip_prefix('#') {
            self.comment(comment.trim());
            return Ok(());
        }
        let Some(columns) = &self.columns else {
            let columns = line
                .split(',')
                .map(|name| {
                    let (variable, element) = parse_column(name.trim().trim_matches('"'))?;
                    Ok((!(self.skip)(&variable)).then_some((variable, element)))
                })
                .collect::<Result<Vec<_>>>()?;
            self.values = vec![Vec::new(); columns.len()];
            self.columns = Some(columns);
            return Ok(());
        };
        self.rows += 1;
        let mut cells = 0;
        for ((cell, column), values) in line.split(',').zip(columns).zip(&mut self.values) {
            cells += 1;
            if column.is_none() {
                continue;
            }
            values.push(parse_number(cell.trim()).ok_or_else(|| {
                Error::msg(format!(
                    "row {} of chain {}'s output has `{cell}`, which is not a number",
                    self.rows, self.chain
                ))
            })?);
        }
        let all = line.split(',').count();
        if cells != columns.len() || all != columns.len() {
            return Err(Error::msg(format!(
                "row {} of chain {}'s output has {all} values for {} columns",
                self.rows,
                self.chain,
                columns.len()
            )));
        }
        Ok(())
    }

    /// One comment's text, `#` and the surrounding space removed.
    fn comment(&mut self, text: &str) {
        if text == "Adaptation terminated" {
            self.section = Section::Adaptation;
            return;
        }
        if let Some(rest) = text.strip_prefix("Elapsed Time:") {
            self.section = Section::Timing;
            self.timing_line(rest);
            return;
        }
        match self.section {
            Section::Adaptation => {
                if let Some(step) = text.strip_prefix("Step size =") {
                    self.step_size = step.trim().parse().ok();
                } else if text.starts_with("Diagonal elements of inverse mass matrix") {
                    self.section = Section::Metric { dense: false };
                } else if text.starts_with("Elements of inverse mass matrix") {
                    self.section = Section::Metric { dense: true };
                }
            }
            Section::Metric { dense } => match numbers(text) {
                Some(row) => {
                    self.metric.push(row);
                    if !dense {
                        self.section = Section::Other;
                    }
                }
                None => self.section = Section::Other,
            },
            Section::Timing => {
                if !self.timing_line(text) {
                    self.section = Section::Other;
                }
            }
            Section::Other => {}
        }
    }

    /// `0.003 seconds (Sampling)`, recorded; false for anything else.
    fn timing_line(&mut self, text: &str) -> bool {
        let Some((seconds, rest)) = text.trim().split_once(" seconds") else {
            return false;
        };
        let Ok(seconds) = seconds.trim().parse::<f64>() else {
            return false;
        };
        let label = rest
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();
        self.timing.insert(label.to_owned(), seconds);
        true
    }

    fn finish(self) -> Result<ChainOutput> {
        let chain = self.chain;
        let Some(columns) = self.columns else {
            return Err(Error::msg("CmdStan's output has no header line"));
        };
        let mut out = Vec::with_capacity(columns.len() * 2);
        // The same variable and element twice would be two series claiming
        // one row of the draws table; CmdStan never writes it, so it is
        // refused.
        let mut seen = BTreeMap::new();
        for (column, mut draws) in columns.into_iter().zip(self.values) {
            let Some((variable, element)) = column else {
                continue;
            };
            if seen
                .insert((variable.clone(), element.clone()), ())
                .is_some()
            {
                return Err(Error::msg(format!(
                    "chain {chain}'s output has the column for {variable}{element:?} twice"
                )));
            }
            let split = self.warmup_rows.min(draws.len());
            let sampling = draws.split_off(split);
            if split > 0 {
                out.push(DrawSeries::new(variable.clone(), element.clone(), chain, draws).warmup());
            }
            out.push(DrawSeries::new(variable, element, chain, sampling));
        }
        Ok(ChainOutput {
            draws: out,
            adaptation: self.step_size.map(|step_size| Adaptation {
                step_size,
                inverse_metric: self.metric,
            }),
            timing: self.timing,
        })
    }
}

/// A comment line of numbers, `0.49, 0.51`, or `None`.
fn numbers(text: &str) -> Option<Vec<f64>> {
    let row = text
        .split(',')
        .map(|n| n.trim().parse::<f64>().ok())
        .collect::<Option<Vec<f64>>>()?;
    (!row.is_empty()).then_some(row)
}

/// A CSV's contents as chain `chain`: every column not `skip`ped as a series,
/// the first `warmup_rows` rows marked as warmup (and a series of their own),
/// the adaptation and the timing.
pub fn read_output(
    text: &str,
    chain: u32,
    warmup_rows: usize,
    skip: &dyn Fn(&str) -> bool,
) -> Result<ChainOutput> {
    let mut reader = reader(chain, warmup_rows, skip);
    for line in text.lines() {
        reader.line(line)?;
    }
    reader.finish()
}

/// A CSV file's contents (see [`read_output`]), read a line at a time.
pub fn read_output_file(
    path: &Path,
    chain: u32,
    warmup_rows: usize,
    skip: &dyn Fn(&str) -> bool,
) -> Result<ChainOutput> {
    let context = || format!("reading CmdStan's output {}", path.display());
    let file = std::fs::File::open(path).with_context(context)?;
    let mut reader = reader(chain, warmup_rows, skip);
    for line in std::io::BufReader::new(file).lines() {
        reader.line(&line.with_context(context)?)?;
    }
    reader.finish()
}

/// A CSV's draws, every column kept (see [`read_output`]).
pub fn read_draws(text: &str, chain: u32, warmup_rows: usize) -> Result<Vec<DrawSeries>> {
    Ok(read_output(text, chain, warmup_rows, &|_| false)?.draws)
}

fn reader(chain: u32, warmup_rows: usize, skip: &dyn Fn(&str) -> bool) -> Reader<'_> {
    Reader {
        chain,
        warmup_rows,
        skip,
        columns: None,
        values: Vec::new(),
        rows: 0,
        section: Section::Other,
        step_size: None,
        metric: Vec::new(),
        timing: BTreeMap::new(),
    }
}

/// A number as CmdStan writes one, including `nan`, `inf` and `-inf`.
fn parse_number(cell: &str) -> Option<f64> {
    match cell.to_ascii_lowercase().as_str() {
        "nan" | "-nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

/// The optimiser's iteration count, from its output: the last line under an
/// `Iter  log prob …` header whose first word is a number. `None` when there
/// is none (an optimiser that did not report, or another method's log).
pub fn optimizer_iterations(log: &str) -> Option<u64> {
    let mut after_header = false;
    let mut last = None;
    for line in log.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"Iter") && line.contains("log prob") {
            after_header = true;
            continue;
        }
        if after_header {
            if let Some(n) = words.first().and_then(|w| w.parse::<u64>().ok()) {
                last = Some(n);
            }
            after_header = false;
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "# model = radon_model\n\
                       # method = sample (Default)\n\
                       lp__,accept_stat__,mu,Sigma.1.1,Sigma.2.1,Sigma.1.2,Sigma.2.2\n\
                       -1.5,0.9,0.1,1,0.5,0.5,2\n\
                       # Adaptation terminated\n\
                       # Step size = 0.912345\n\
                       # Diagonal elements of inverse mass matrix:\n\
                       # 0.5, 0.25, 1, 1, 2\n\
                       -1.25,0.8,0.2,1.1,0.6,0.6,2.1\n\
                       -1,0.7,nan,1.2,inf,-inf,2.2\n\
                       # \n\
                       #  Elapsed Time: 0.002 seconds (Warm-up)\n\
                       #                0.003 seconds (Sampling)\n\
                       #                0.005 seconds (Total)\n\
                       # \n";

    #[test]
    fn columns_are_named_elements_and_warmup_rows_are_split_off() {
        let draws = read_draws(CSV, 3, 1).expect("draws");
        let labels: Vec<String> = draws
            .iter()
            .filter(|s| !s.warmup)
            .map(DrawSeries::label)
            .collect();
        assert_eq!(
            labels,
            [
                "lp__",
                "accept_stat__",
                "mu",
                "Sigma[1,1]",
                "Sigma[2,1]",
                "Sigma[1,2]",
                "Sigma[2,2]"
            ]
        );
        let sigma21 = draws
            .iter()
            .find(|s| s.label() == "Sigma[2,1]" && !s.warmup)
            .expect("Sigma[2,1]");
        assert_eq!(sigma21.chain, 3);
        assert_eq!(sigma21.draws[0], 0.6);
        assert!(sigma21.draws[1].is_infinite());
        let warm = draws
            .iter()
            .find(|s| s.label() == "lp__" && s.warmup)
            .expect("warmup lp__");
        assert_eq!(warm.draws, vec![-1.5]);
        assert!(
            draws
                .iter()
                .find(|s| s.label() == "mu" && !s.warmup)
                .expect("mu")
                .draws[1]
                .is_nan()
        );
    }

    #[test]
    fn the_adaptation_and_the_timing_are_read_from_the_comments() {
        let out = read_output(CSV, 1, 0, &|_| false).expect("read");
        assert_eq!(
            out.adaptation,
            Some(Adaptation {
                step_size: 0.912345,
                inverse_metric: vec![vec![0.5, 0.25, 1.0, 1.0, 2.0]],
            })
        );
        assert_eq!(
            out.timing,
            BTreeMap::from([
                ("Warm-up".to_owned(), 0.002),
                ("Sampling".to_owned(), 0.003),
                ("Total".to_owned(), 0.005),
            ])
        );
        // A dense metric is a row per parameter; Pathfinder's timing has its
        // own labels, and its rows a space after each comma.
        let dense = "a,b\n# Adaptation terminated\n# Step size = 0.5\n\
                     # Elements of inverse mass matrix:\n# 1, 0.1\n# 0.1, 2\n1,2\n";
        let out = read_output(dense, 1, 0, &|_| false).expect("dense");
        let metric = out.adaptation.expect("adapted").inverse_metric;
        assert_eq!(metric, vec![vec![1.0, 0.1], vec![0.1, 2.0]]);
        let pathfinder = "lp_approx__,lp__,path__,theta\n-0.8, -7.0, 1, 0.16\n\
                          # Elapsed Time: 0.000000 seconds (Pathfinders)\n\
                          #               0.001000 seconds (PSIS)\n\
                          #               0.001000 seconds (Total)\n";
        let out = read_output(pathfinder, 1, 0, &|_| false).expect("pathfinder");
        assert_eq!(out.adaptation, None);
        assert_eq!(out.timing["PSIS"], 0.001);
        assert_eq!(out.draws[3].draws, vec![0.16]);
    }

    #[test]
    fn a_skipped_variable_is_not_kept() {
        let out = read_output(CSV, 1, 0, &|v| v == "Sigma").expect("read");
        let names: Vec<String> = out.draws.iter().map(DrawSeries::label).collect();
        assert_eq!(names, ["lp__", "accept_stat__", "mu"]);
        // The rows are still checked whole.
        assert!(read_output("a,b\n1,2,3\n", 1, 0, &|v| v == "b").is_err());
    }

    #[test]
    fn a_ragged_row_or_a_bad_index_is_refused() {
        assert!(read_draws("a,b\n1\n", 1, 0).is_err());
        assert!(read_draws("a,b\n1,2,3\n", 1, 0).is_err());
        assert!(read_draws("a.x\n1\n", 1, 0).is_err());
        assert!(read_draws("# nothing\n", 1, 0).is_err());
    }

    #[test]
    fn the_optimisers_iterations_are_its_last_numbered_line() {
        // CmdStan 2.40's `optimize` output, refresh=1.
        let log = "Initial log joint probability = -9.51104\n\
                   \x20   Iter      log prob        ||dx||      ||grad||       alpha      alpha0  # evals  Notes \n\
                   \x20      1      -5.86106      0.255143        1.9292         0.1       0.001        4   \n\
                   \x20   Iter      log prob        ||dx||      ||grad||       alpha      alpha0  # evals  Notes \n\
                   \x20      5      -5.00402    0.00145194   2.24565e-05           1           1        8   \n\
                   Optimization terminated normally: \n";
        assert_eq!(optimizer_iterations(log), Some(5));
        assert_eq!(optimizer_iterations("Iteration: 1 / 20 [  5%]"), None);
    }
}
