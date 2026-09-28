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
//! - with `save_warmup`, the first `⌈iter_warmup / thin⌉` rows are warmup.
//!
//! The adaptation and timing comments are not read yet (TODO 5.1).

use std::collections::BTreeMap;
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

/// A CSV's draws, one series per column, as chain `chain`: the first
/// `warmup_rows` rows marked as warmup (and a series of their own), the rest
/// not.
pub fn read_draws(text: &str, chain: u32, warmup_rows: usize) -> Result<Vec<DrawSeries>> {
    let mut rows = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let Some(header) = rows.next() else {
        return Err(Error::msg("CmdStan's output has no header line"));
    };
    let columns = header
        .split(',')
        .map(|name| parse_column(name.trim().trim_matches('"')))
        .collect::<Result<Vec<_>>>()?;
    let mut values: Vec<Vec<f64>> = vec![Vec::new(); columns.len()];
    for (n, row) in rows.enumerate() {
        let cells: Vec<&str> = row.split(',').collect();
        if cells.len() != columns.len() {
            return Err(Error::msg(format!(
                "row {} of chain {chain}'s output has {} values for {} columns",
                n + 1,
                cells.len(),
                columns.len()
            )));
        }
        for (column, cell) in values.iter_mut().zip(cells) {
            column.push(parse_number(cell.trim()).ok_or_else(|| {
                Error::msg(format!(
                    "row {} of chain {chain}'s output has `{cell}`, which is not a number",
                    n + 1
                ))
            })?);
        }
    }
    let mut out = Vec::with_capacity(columns.len() * 2);
    // The same variable and element twice would be two series claiming one
    // row of the draws table; CmdStan never writes it, so it is refused.
    let mut seen = BTreeMap::new();
    for ((variable, element), mut draws) in columns.into_iter().zip(values) {
        if seen
            .insert((variable.clone(), element.clone()), ())
            .is_some()
        {
            return Err(Error::msg(format!(
                "chain {chain}'s output has the column for {variable}{element:?} twice"
            )));
        }
        let split = warmup_rows.min(draws.len());
        let sampling = draws.split_off(split);
        if split > 0 {
            out.push(DrawSeries::new(variable.clone(), element.clone(), chain, draws).warmup());
        }
        out.push(DrawSeries::new(variable, element, chain, sampling));
    }
    Ok(out)
}

/// A CSV file's draws (see [`read_draws`]).
pub fn read_draws_file(path: &Path, chain: u32, warmup_rows: usize) -> Result<Vec<DrawSeries>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading CmdStan's output {}", path.display()))?;
    read_draws(&text, chain, warmup_rows)
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

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "# model = radon_model\n\
                       # method = sample (Default)\n\
                       lp__,accept_stat__,mu,Sigma.1.1,Sigma.2.1,Sigma.1.2,Sigma.2.2\n\
                       -1.5,0.9,0.1,1,0.5,0.5,2\n\
                       # Adaptation terminated\n\
                       -1.25,0.8,0.2,1.1,0.6,0.6,2.1\n\
                       -1,0.7,nan,1.2,inf,-inf,2.2\n\
                       # Elapsed Time: 0.1 seconds (Warm-up)\n";

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
    fn a_ragged_row_or_a_bad_index_is_refused() {
        assert!(read_draws("a,b\n1\n", 1, 0).is_err());
        assert!(read_draws("a.x\n1\n", 1, 0).is_err());
        assert!(read_draws("# nothing\n", 1, 0).is_err());
    }
}
