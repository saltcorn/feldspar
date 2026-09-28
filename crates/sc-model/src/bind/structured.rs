//! The structured binding kinds' arithmetic (Stan TODO §12): rows aggregated
//! into the positions of one or two dimensions, a graph over a dimension, and
//! sites on the globe.
//!
//! Pure functions over positions and numbers. The resolver has already looked
//! every row up in its dimensions (and applied the `unknown` policy); what
//! reaches here is 0-based positions, and what leaves is numbers or a
//! sentence fragment the resolver puts after the variable it is about.

use nalgebra::DMatrix;

use super::spec::{Aggregate, Symmetric};
use super::tensor::Values;

/// The most nodes `icar_scale` decomposes: the eigendecomposition is cubic,
/// and 5 000 regions is already minutes.
pub const MAX_ICAR_NODES: usize = 5_000;

/// The most sites `distances` computes between: 3 000 is nine million
/// distances, which is most of the default data-values cap on its own.
pub const MAX_DISTANCE_SITES: usize = 3_000;

/// The mean radius of the Earth, in km (IUGG).
const EARTH_RADIUS_KM: f64 = 6_371.008_8;

/// A column's values with its nulls kept — what a `series` aggregates.
#[derive(Debug, Clone)]
pub(crate) enum Optional {
    Int(Vec<Option<i64>>),
    Real(Vec<Option<f64>>),
}

impl Optional {
    fn is_present(&self, i: usize) -> bool {
        match self {
            Optional::Int(v) => v[i].is_some(),
            Optional::Real(v) => v[i].is_some(),
        }
    }
}

/// Where each row fell — the position of its cell, `None` for a row that
/// falls nowhere (a null where it is looked up) — and how to name a cell and
/// a row in a sentence.
pub(crate) struct Cells<'a> {
    /// How many cells: a dimension's size, or `R × C`, row-major.
    pub count: usize,
    /// Each row's cell.
    pub cell_of: &'a [Option<usize>],
    /// `2024-03-02` of `day`; (`Aitkin`, `2024-03-02`).
    pub cell_name: &'a dyn Fn(usize) -> String,
    /// A row's key.
    pub row_name: &'a dyn Fn(usize) -> String,
    /// The dataset the rows are of.
    pub dataset: &'a str,
}

/// The rows aggregated into their cells, with `fill` where none falls.
/// Errors are sentence fragments.
pub(crate) fn aggregate(
    cells: &Cells<'_>,
    values: Option<&Optional>,
    how: Aggregate,
    fill: Option<f64>,
) -> Result<Values, String> {
    let n = cells.count;
    // The rows of each cell, in the dataset's order; a row with a null value
    // is not in its cell, because it has nothing to say there.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (row, cell) in cells.cell_of.iter().enumerate() {
        let Some(cell) = cell else { continue };
        if values.is_some_and(|v| !v.is_present(row)) {
            continue;
        }
        members[*cell].push(row);
    }

    if how == Aggregate::Count {
        return Ok(Values::Int(
            members.iter().map(|m| m.len() as i64).collect(),
        ));
    }
    let Some(values) = values else {
        return Err(format!(
            "`{}` needs a `column` to aggregate; with none, rows are counted",
            how.name()
        ));
    };
    if how == Aggregate::Refuse {
        if let Some(cell) = members.iter().position(|m| m.len() > 1) {
            return Err(format!(
                "rows `{}` and `{}` of `{}` both fall in {}; give an `aggregate` (`sum`, \
                 `mean`, `min`, `max`, `first`, `last`), or filter the dataset so each \
                 position has one row",
                (cells.row_name)(members[cell][0]),
                (cells.row_name)(members[cell][1]),
                cells.dataset,
                (cells.cell_name)(cell),
            ));
        }
    }
    let empty: Vec<usize> = (0..n).filter(|c| members[*c].is_empty()).collect();
    if let (Some(&first), None) = (empty.first(), fill) {
        let others = match empty.len() - 1 {
            0 => String::new(),
            1 => " (and so does 1 other position)".to_owned(),
            k => format!(" (and so do {k} other positions)"),
        };
        return Err(format!(
            "{} has no value{others}; give a `fill` — and bind `series_present` or \
             `cells_present` for the mask — or bind the observed positions with `index` and \
             their values with `column`, Stan's missing-data idiom",
            (cells.cell_name)(first)
        ));
    }

    let int_fill = fill.filter(|f| f.fract() == 0.0 && f.is_finite() && f.abs() < 9e15);
    match values {
        Optional::Int(v) if how != Aggregate::Mean && (empty.is_empty() || int_fill.is_some()) => {
            let fill = int_fill.unwrap_or_default() as i64;
            Ok(Values::Int(
                members
                    .iter()
                    .map(|m| {
                        let mut xs = m.iter().filter_map(|r| v[*r]);
                        if m.is_empty() {
                            return fill;
                        }
                        match how {
                            Aggregate::Sum => xs.sum(),
                            Aggregate::Min => xs.min().unwrap_or(fill),
                            Aggregate::Max => xs.max().unwrap_or(fill),
                            Aggregate::Last => xs.next_back().unwrap_or(fill),
                            _ => xs.next().unwrap_or(fill),
                        }
                    })
                    .collect(),
            ))
        }
        _ => {
            let fill = fill.unwrap_or(f64::NAN);
            let number = |r: usize| match values {
                Optional::Int(v) => v[r].map(|x| x as f64),
                Optional::Real(v) => v[r],
            };
            Ok(Values::Real(
                members
                    .iter()
                    .map(|m| {
                        if m.is_empty() {
                            return fill;
                        }
                        let mut xs = m.iter().filter_map(|r| number(*r));
                        match how {
                            Aggregate::Sum => xs.sum(),
                            Aggregate::Mean => xs.sum::<f64>() / m.len() as f64,
                            Aggregate::Min => xs.fold(f64::INFINITY, f64::min),
                            Aggregate::Max => xs.fold(f64::NEG_INFINITY, f64::max),
                            Aggregate::Last => xs.next_back().unwrap_or(fill),
                            _ => xs.next().unwrap_or(fill),
                        }
                    })
                    .collect(),
            ))
        }
    }
}

/// 1 for each cell some row (with a value, when there are values) falls in,
/// 0 for the others.
pub(crate) fn present(cells: &Cells<'_>, values: Option<&Optional>) -> Values {
    let mut seen = vec![0i64; cells.count];
    for (row, cell) in cells.cell_of.iter().enumerate() {
        if let Some(cell) = cell {
            if values.is_none_or(|v| v.is_present(row)) {
                seen[*cell] = 1;
            }
        }
    }
    Values::Int(seen)
}

/// A graph over the `n` positions of a dimension, from a junction table's
/// rows: each row's two ends, 0-based, in the dataset's order.
#[derive(Debug, Clone)]
pub(crate) struct Graph {
    pub nodes: usize,
    /// Each row's ends, as stored.
    pub rows: Vec<(usize, usize)>,
}

impl Graph {
    /// The edge list: each unordered pair once with the lesser end first,
    /// sorted — or every row as stored. 1-based, as Stan indexes.
    pub(crate) fn edges(&self, symmetric: Symmetric) -> Vec<(i64, i64)> {
        let mut edges: Vec<(i64, i64)> = self
            .rows
            .iter()
            .map(|(a, b)| (*a as i64 + 1, *b as i64 + 1))
            .collect();
        if symmetric == Symmetric::Dedupe {
            for e in &mut edges {
                if e.0 > e.1 {
                    *e = (e.1, e.0);
                }
            }
            edges.sort_unstable();
            edges.dedup();
        }
        edges
    }

    /// The dense 0/1 adjacency, symmetric, row-major.
    pub(crate) fn adjacency(&self) -> Vec<i64> {
        let n = self.nodes;
        let mut w = vec![0i64; n * n];
        for (a, b) in &self.rows {
            w[a * n + b] = 1;
            w[b * n + a] = 1;
        }
        w
    }

    /// Each node's neighbours, each once.
    fn neighbours(&self) -> Vec<Vec<usize>> {
        let mut out = vec![Vec::new(); self.nodes];
        for (a, b) in &self.rows {
            out[*a].push(*b);
            out[*b].push(*a);
        }
        for list in &mut out {
            list.sort_unstable();
            list.dedup();
        }
        out
    }

    /// Each node's component, numbered from 0 in the order of its first node.
    pub(crate) fn components(&self) -> Vec<usize> {
        let neighbours = self.neighbours();
        let mut component = vec![usize::MAX; self.nodes];
        let mut next = 0;
        for start in 0..self.nodes {
            if component[start] != usize::MAX {
                continue;
            }
            component[start] = next;
            let mut stack = vec![start];
            while let Some(node) = stack.pop() {
                for &m in &neighbours[node] {
                    if component[m] == usize::MAX {
                        component[m] = next;
                        stack.push(m);
                    }
                }
            }
            next += 1;
        }
        component
    }

    /// The nodes with no neighbour.
    pub(crate) fn isolated(&self) -> Vec<usize> {
        self.neighbours()
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_empty())
            .map(|(i, _)| i)
            .collect()
    }

    /// BYM2's scaling factor (Riebler et al. 2016; Morris et al. 2019): the
    /// geometric mean, over every node, of the marginal variance of the ICAR
    /// model — the diagonal of the generalised inverse of `Q = D − W`, taken
    /// per connected component, where a singleton component's variance is 1.
    ///
    /// The generalised inverse is the Moore–Penrose one from `nalgebra`'s
    /// symmetric eigendecomposition: a connected component's Laplacian has
    /// exactly one zero eigenvalue (the constant vector, the sum-to-zero
    /// constraint's direction), and every other one is inverted.
    pub(crate) fn icar_scale(&self) -> f64 {
        let component = self.components();
        let neighbours = self.neighbours();
        let count = component.iter().max().map_or(0, |m| m + 1);
        let mut log_sum = 0.0;
        for c in 0..count {
            let nodes: Vec<usize> = (0..self.nodes).filter(|i| component[*i] == c).collect();
            let m = nodes.len();
            if m == 1 {
                // log 1 = 0.
                continue;
            }
            let q = DMatrix::<f64>::from_fn(m, m, |i, j| {
                if i == j {
                    neighbours[nodes[i]].len() as f64
                } else if neighbours[nodes[i]].contains(&nodes[j]) {
                    -1.0
                } else {
                    0.0
                }
            });
            let eigen = q.symmetric_eigen();
            let null = eigen
                .eigenvalues
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .map_or(0, |(k, _)| k);
            for i in 0..m {
                let variance: f64 = (0..m)
                    .filter(|k| *k != null)
                    .map(|k| eigen.eigenvectors[(i, k)].powi(2) / eigen.eigenvalues[k])
                    .sum();
                log_sum += variance.ln();
            }
        }
        if self.nodes == 0 {
            return 1.0;
        }
        (log_sum / self.nodes as f64).exp()
    }
}

/// Sites as `[x, y]` row-major: `[lon, lat]` in degrees, or projected to km
/// east and north of the centroid (equirectangular: adequate at
/// city-to-country scale, wrong across the antimeridian or near a pole).
pub(crate) fn points(sites: &[(f64, f64)], project: bool) -> Vec<f64> {
    if !project {
        return sites.iter().flat_map(|(lat, lon)| [*lon, *lat]).collect();
    }
    let n = sites.len().max(1) as f64;
    let lat0 = sites.iter().map(|s| s.0).sum::<f64>() / n;
    let lon0 = sites.iter().map(|s| s.1).sum::<f64>() / n;
    let k = EARTH_RADIUS_KM * std::f64::consts::PI / 180.0;
    let cos0 = lat0.to_radians().cos();
    sites
        .iter()
        .flat_map(|(lat, lon)| [k * (lon - lon0) * cos0, k * (lat - lat0)])
        .collect()
}

/// The great-circle distances between every pair of sites, in km
/// (haversine), row-major.
pub(crate) fn distances(sites: &[(f64, f64)]) -> Vec<f64> {
    let n = sites.len();
    let mut out = vec![0.0; n * n];
    for i in 0..n {
        for j in (i + 1)..n {
            let d = great_circle(sites[i], sites[j]);
            out[i * n + j] = d;
            out[j * n + i] = d;
        }
    }
    out
}

fn great_circle((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: usize, rows: &[(usize, usize)]) -> Graph {
        Graph {
            nodes,
            rows: rows.to_vec(),
        }
    }

    /// The `side × side` rook lattice, each pair stored once.
    fn lattice(side: usize) -> Graph {
        let mut rows = Vec::new();
        for r in 0..side {
            for c in 0..side {
                let i = r * side + c;
                if c + 1 < side {
                    rows.push((i, i + 1));
                }
                if r + 1 < side {
                    rows.push((i, i + side));
                }
            }
        }
        graph(side * side, &rows)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-10 * b.abs().max(1.0)
    }

    #[test]
    fn the_icar_scale_matches_the_closed_forms() {
        // Two nodes: Q = [[1, -1], [-1, 1]], Q⁺ = Q / 4, so each variance is
        // 1/4.
        assert!(close(graph(2, &[(0, 1)]).icar_scale(), 0.25));
        // The complete graph K_n: L⁺ = (I − J/n) / n, each variance
        // (n − 1) / n².
        let k4 = graph(4, &[(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)]);
        assert!(close(k4.icar_scale(), 3.0 / 16.0), "{}", k4.icar_scale());
        // The cycle C_n: each variance (n² − 1) / (12 n) — for C_6, 35/72.
        let c6 = graph(6, &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 0)]);
        assert!(close(c6.icar_scale(), 35.0 / 72.0), "{}", c6.icar_scale());
        // The path P_3: variances 5/9, 2/9, 5/9.
        let p3 = graph(3, &[(0, 1), (1, 2)]);
        let expected = (5.0f64 / 9.0 * 2.0 / 9.0 * 5.0 / 9.0).cbrt();
        assert!(close(p3.icar_scale(), expected), "{}", p3.icar_scale());
    }

    #[test]
    fn the_icar_scale_of_a_four_by_four_lattice() {
        // Computed independently and exactly, with no eigendecomposition: for
        // a connected graph L⁺ = (L + J/n)⁻¹ − J/n, inverted by Gauss–Jordan
        // over Python's `fractions.Fraction`, then
        // `math.exp(sum(math.log(v) for v in diag) / n)` — the script builds
        // the rook lattice as `lattice` below does. (It gives 0.3125 for the
        // 2 × 2 lattice, which is C_4's (16 − 1) / 48.)
        const LATTICE_4X4: f64 = 0.471_629_643_725_645_3;
        let scale = lattice(4).icar_scale();
        assert!(close(scale, LATTICE_4X4), "{scale}");
        // The order the pairs are stored in, and storing both directions,
        // change nothing.
        let mut both = lattice(4);
        let reversed: Vec<(usize, usize)> = both.rows.iter().map(|(a, b)| (*b, *a)).collect();
        both.rows.extend(reversed);
        assert!(close(both.icar_scale(), LATTICE_4X4));
    }

    #[test]
    fn a_singleton_component_contributes_one_and_each_component_is_its_own() {
        // Two separate pairs and an island: variances 1/4, 1/4, 1/4, 1/4, 1.
        let g = graph(5, &[(0, 1), (2, 3)]);
        assert_eq!(g.components(), [0, 0, 1, 1, 2]);
        assert_eq!(g.isolated(), [4]);
        let expected = (4.0 * 0.25f64.ln() / 5.0).exp();
        assert!(close(g.icar_scale(), expected), "{}", g.icar_scale());
    }

    #[test]
    fn an_edge_list_dedupes_unordered_pairs_or_keeps_rows_as_stored() {
        let g = graph(4, &[(2, 1), (1, 2), (0, 3), (1, 0)]);
        assert_eq!(g.edges(Symmetric::Dedupe), [(1, 2), (1, 4), (2, 3)]);
        assert_eq!(g.edges(Symmetric::Keep), [(3, 2), (2, 3), (1, 4), (2, 1)]);
        assert_eq!(
            g.adjacency(),
            [0, 1, 0, 1, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0]
        );
    }

    #[test]
    fn great_circle_distances_and_the_projection() {
        // London to Paris: 343.5 km by the haversine on the mean radius.
        let sites = [(51.5074, -0.1278), (48.8566, 2.3522)];
        let d = distances(&sites);
        assert_eq!(d[0], 0.0);
        assert!((d[1] - 343.56).abs() < 0.1, "{}", d[1]);
        assert_eq!(d[1], d[2]);
        // A degree of latitude is 111.195 km on the mean radius; projected
        // points are centred on the centroid.
        let p = points(&[(0.0, 0.0), (1.0, 0.0)], true);
        assert!((p[3] - p[1] - 111.195).abs() < 1e-3, "{p:?}");
        assert!((p[1] + p[3]).abs() < 1e-9);
        assert_eq!(points(&[(10.0, 20.0)], false), [20.0, 10.0]);
    }

    #[test]
    fn aggregation_counts_combines_fills_and_refuses() {
        let cell_name = |c: usize| format!("`{c}`");
        let row_name = |r: usize| format!("{}", r + 1);
        let cell_of = [Some(0), Some(2), Some(0), None, Some(2)];
        let cells = Cells {
            count: 4,
            cell_of: &cell_of,
            cell_name: &cell_name,
            row_name: &row_name,
            dataset: "main",
        };
        let v = Optional::Int(vec![Some(1), Some(5), Some(3), Some(9), None]);
        assert_eq!(
            aggregate(&cells, None, Aggregate::Count, None).unwrap(),
            Values::Int(vec![2, 0, 2, 0])
        );
        // A null value is not counted when there is a column.
        assert_eq!(
            aggregate(&cells, Some(&v), Aggregate::Count, None).unwrap(),
            Values::Int(vec![2, 0, 1, 0])
        );
        assert_eq!(
            aggregate(&cells, Some(&v), Aggregate::Sum, Some(0.0)).unwrap(),
            Values::Int(vec![4, 0, 5, 0])
        );
        assert_eq!(
            aggregate(&cells, Some(&v), Aggregate::Mean, Some(f64::NAN))
                .unwrap()
                .number(0),
            Some(2.0)
        );
        assert_eq!(
            aggregate(&cells, Some(&v), Aggregate::Last, Some(-1.0)).unwrap(),
            Values::Int(vec![3, -1, 5, -1])
        );
        // A fractional fill makes the series real.
        assert!(matches!(
            aggregate(&cells, Some(&v), Aggregate::Max, Some(0.5)).unwrap(),
            Values::Real(_)
        ));
        let err = aggregate(&cells, Some(&v), Aggregate::Refuse, Some(0.0)).unwrap_err();
        assert!(
            err.contains("rows `1` and `3` of `main` both fall in `0`"),
            "{err}"
        );
        let err = aggregate(&cells, Some(&v), Aggregate::First, None).unwrap_err();
        assert!(
            err.contains("`1` has no value (and so does 1 other position); give a `fill`"),
            "{err}"
        );
        assert_eq!(present(&cells, Some(&v)), Values::Int(vec![1, 0, 1, 0]));
        assert_eq!(present(&cells, None), Values::Int(vec![1, 0, 1, 0]));
    }
}
