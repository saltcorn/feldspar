//! Choosing the plot from what was dropped (analytics TODO A2.2): the "show
//! me" rules, which pick a mark and a stat from the types of the columns on the
//! drop zones, and the gallery's presets, each a function from a dataset's
//! shape (and what is already dropped) to a spec.
//!
//! Users never meet the words "mark" or "stat": they drop `price` on Y and
//! `neighbourhood` on X and get a box plot, because a number by a category is a
//! box plot (the goals document's table). The mark palette is the one place a
//! person overrides the choice, and [`show_me`] honours it with the stat that
//! mark needs.
//!
//! The rules in brief, by what is on X and Y (`C` a number, `D` a category or
//! a binned number, `T` a date):
//!
//! | X | Y | plot |
//! |---|---|---|
//! | C | — | histogram (X binned, counted) |
//! | D | — | bar chart of counts |
//! | T | — | line of counts |
//! | — | C or D | the same, across |
//! | C | C | scatter plot |
//! | D | C | box plot |
//! | C | D | box plot, flipped |
//! | T | C | line of the mean |
//! | C | T | scatter plot |
//! | D or T | D | heatmap of counts |
//!
//! Several columns on Y are folded into `value` and `variable`, and
//! `variable` goes on Color (or Wrap, when Color is taken).

use sc_dataset::{ColType, Grain, StageShape};
use serde::{Deserialize, Serialize};

use super::render::MAX_SAMPLE;
use super::spec::{
    AggregateFn, Channel, Coord, DataRef, Facet, FacetScales, FieldDef, Fold, Layer, Mark,
    PlotSpec, Scale, Stat,
};

/// The columns on the explorer's drop zones.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// X.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<FieldDef>,
    /// Y: several are compared as one variable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub y: Vec<FieldDef>,
    /// Color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<FieldDef>,
    /// Size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<FieldDef>,
    /// Shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<FieldDef>,
    /// Label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<FieldDef>,
    /// Facet rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<FieldDef>,
    /// Facet columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<FieldDef>,
    /// Wrap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<FieldDef>,
}

/// A gallery item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// The distribution of a number.
    Histogram,
    /// Counts or means by category.
    Bar,
    /// A number over time or over another number.
    Line,
    /// Two numbers.
    Scatter,
    /// A number by category.
    Box,
    /// Counts by two categories.
    Heatmap,
    /// A line filled to zero.
    Area,
    /// Every pair of several numbers, a scatter plot each (A2.11).
    Splom,
    /// Several numbers, each row a line across one axis per number.
    Parallel,
    /// The correlation of every pair of several numbers.
    Correlation,
    /// Counts by two categories, as tiles in proportion.
    Mosaic,
    /// Points or regions on a map (A5).
    Map,
}

impl Preset {
    /// Every item, in gallery order.
    pub const ALL: [Preset; 12] = [
        Preset::Histogram,
        Preset::Bar,
        Preset::Line,
        Preset::Scatter,
        Preset::Box,
        Preset::Heatmap,
        Preset::Area,
        Preset::Splom,
        Preset::Parallel,
        Preset::Correlation,
        Preset::Mosaic,
        Preset::Map,
    ];

    /// Whether it reshapes the data itself, so that what is dropped later is
    /// read by it again rather than by the "show me" rules.
    pub fn reshapes(self) -> bool {
        matches!(
            self,
            Preset::Splom | Preset::Parallel | Preset::Correlation | Preset::Mosaic
        )
    }

    /// Its name as the JSON spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Histogram => "histogram",
            Preset::Bar => "bar",
            Preset::Line => "line",
            Preset::Scatter => "scatter",
            Preset::Box => "box",
            Preset::Heatmap => "heatmap",
            Preset::Area => "area",
            Preset::Splom => "splom",
            Preset::Parallel => "parallel",
            Preset::Correlation => "correlation",
            Preset::Mosaic => "mosaic",
            Preset::Map => "map",
        }
    }

    /// Its English name (the Analytics UI translates by [`as_str`](Self::as_str)).
    pub fn label(self) -> &'static str {
        match self {
            Preset::Histogram => "Histogram",
            Preset::Bar => "Bar chart",
            Preset::Line => "Line chart",
            Preset::Scatter => "Scatter plot",
            Preset::Box => "Box plot",
            Preset::Heatmap => "Heatmap",
            Preset::Area => "Area chart",
            Preset::Splom => "Scatterplot matrix",
            Preset::Parallel => "Parallel coordinates",
            Preset::Correlation => "Correlation heatmap",
            Preset::Mosaic => "Mosaic plot",
            Preset::Map => "Map",
        }
    }

    /// The milestone that brings it, when it is not here yet.
    pub fn arrives_in(self) -> Option<&'static str> {
        match self {
            Preset::Map => Some("A5"),
            _ => None,
        }
    }
}

/// One item of the gallery as the explorer lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GalleryItem {
    /// Which preset.
    pub preset: Preset,
    /// Its English name.
    pub label: &'static str,
    /// Whether it can be picked.
    pub available: bool,
    /// Whether it reshapes the data itself ([`Preset::reshapes`]).
    pub reshapes: bool,
    /// The milestone that brings it, when it cannot.
    pub arrives_in: Option<&'static str>,
}

/// The gallery, in order, with the items a later milestone brings disabled.
pub fn gallery() -> Vec<GalleryItem> {
    Preset::ALL
        .into_iter()
        .map(|preset| GalleryItem {
            preset,
            label: preset.label(),
            available: preset.arrives_in().is_none(),
            reshapes: preset.reshapes(),
            arrives_in: preset.arrives_in(),
        })
        .collect()
}

/// What a column on a drop zone is, for choosing a plot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A number, unbinned.
    Continuous,
    /// A category, a boolean, a foreign key, or a binned number.
    Discrete,
    /// A date or a time.
    Temporal,
}

fn kind_of(shape: &StageShape, f: &FieldDef) -> Kind {
    let Some(c) = shape.column(&f.field) else {
        return Kind::Discrete;
    };
    if f.bin.is_some() || c.key.is_some() {
        return Kind::Discrete;
    }
    match c.ty {
        ColType::Int | ColType::Float | ColType::Decimal => Kind::Continuous,
        ColType::Date | ColType::Timestamp | ColType::Time => Kind::Temporal,
        _ => Kind::Discrete,
    }
}

/// The spec for what is on the drop zones, drawn as `mark` when the mark
/// palette chose one, or by the rules above when not. The sentence says what
/// is missing when nothing can be drawn.
pub fn show_me(
    data: DataRef,
    shape: &StageShape,
    assignment: &Assignment,
    mark: Option<Mark>,
) -> Result<PlotSpec, String> {
    let a = assignment;
    let mut fold = None;
    let mut color = a.color.clone();
    let mut wrap = a.wrap.clone();
    let y: Option<FieldDef> = match a.y.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        several => {
            let f = Fold::of(several.iter().map(|f| f.field.clone()));
            let variable = FieldDef::of(f.key.clone());
            if color.is_none() {
                color = Some(variable);
            } else if wrap.is_none() && a.row.is_none() && a.column.is_none() {
                wrap = Some(variable);
            } else {
                return Err(
                    "several columns on Y are told apart by colour or by Wrap, and both are taken"
                        .to_owned(),
                );
            }
            let value = FieldDef::of(f.value.clone());
            fold = Some(f);
            Some(value)
        }
    };
    let shape = &super::validate::folded_shape(shape, fold.as_ref()).map_err(|p| p.join("; "))?;
    let mut x = a.x.clone();
    let mut y = y;
    let kx = x.as_ref().map(|f| kind_of(shape, f));
    let ky = y.as_ref().map(|f| kind_of(shape, f));
    let mut coord = Coord::Cartesian;
    use Kind::{Continuous as C, Discrete as D, Temporal as T};

    let binned = |f: &Option<FieldDef>| {
        f.clone().map(|mut f| {
            if f.bin.is_none() {
                f.bin = Some(Default::default());
            }
            f
        })
    };
    // Swap X and Y and draw flipped: a box plot or bars across.
    let mut flip = |x: &mut Option<FieldDef>, y: &mut Option<FieldDef>| {
        std::mem::swap(x, y);
        coord = Coord::Flipped;
    };

    let (mark, stat) = match mark {
        None => match (kx, ky) {
            (None, None) => return Err("drop a column on X or Y to draw a plot".to_owned()),
            (Some(C), None) => {
                x = binned(&x);
                (Mark::Bar, Stat::Count)
            }
            (Some(D), None) => (Mark::Bar, Stat::Count),
            (Some(T), None) => (Mark::Line, Stat::Count),
            (None, Some(C)) => {
                y = binned(&y);
                (Mark::Bar, Stat::Count)
            }
            (None, Some(_)) => (Mark::Bar, Stat::Count),
            (Some(C), Some(C)) | (Some(C), Some(T)) => (Mark::Point, Stat::Identity),
            (Some(D), Some(C)) => (Mark::Box, Stat::boxplot()),
            (Some(C), Some(D)) => {
                flip(&mut x, &mut y);
                (Mark::Box, Stat::boxplot())
            }
            (Some(T), Some(C)) => (Mark::Line, Stat::aggregate(AggregateFn::Mean)),
            (Some(D | T), Some(D)) | (Some(D), Some(T)) | (Some(T), Some(T)) => {
                if color.is_none() {
                    (Mark::Rect, Stat::Count)
                } else {
                    (Mark::Text, Stat::Count)
                }
            }
        },
        Some(m) => {
            let numeric_y = ky == Some(C);
            match m {
                Mark::Point | Mark::Line | Mark::Area | Mark::Bar => match (kx, ky) {
                    (None, None) => {
                        return Err("drop a column on X or Y to draw a plot".to_owned());
                    }
                    (Some(C), None) => {
                        x = binned(&x);
                        (m, Stat::Count)
                    }
                    (None, Some(C)) => {
                        y = binned(&y);
                        (m, Stat::Count)
                    }
                    (_, None) | (None, _) => (m, Stat::Count),
                    (Some(C), Some(C)) if m == Mark::Bar => {
                        x = binned(&x);
                        (m, Stat::aggregate(AggregateFn::Mean))
                    }
                    (Some(C), Some(C) | Some(T)) => (m, Stat::Identity),
                    (Some(_), Some(C)) if m == Mark::Point => (m, Stat::Identity),
                    (Some(_), Some(C)) => (m, Stat::aggregate(AggregateFn::Mean)),
                    (Some(C), Some(D)) => {
                        flip(&mut x, &mut y);
                        if m == Mark::Point {
                            (m, Stat::Identity)
                        } else {
                            (m, Stat::aggregate(AggregateFn::Mean))
                        }
                    }
                    (Some(_), Some(_)) if m == Mark::Point => (m, Stat::Identity),
                    (Some(_), Some(_)) => (m, Stat::Count),
                },
                Mark::Box | Mark::Band | Mark::Errorbar => {
                    if !numeric_y {
                        if kx == Some(C) && ky != Some(C) {
                            flip(&mut x, &mut y);
                        } else {
                            return Err(format!(
                                "{} need a number on Y",
                                if m == Mark::Box {
                                    "box plots"
                                } else {
                                    "a mean and its interval"
                                }
                            ));
                        }
                    }
                    if x.as_ref().map(|f| kind_of(shape, f)) == Some(C) {
                        x = binned(&x);
                    }
                    let stat = if m == Mark::Box {
                        Stat::boxplot()
                    } else {
                        Stat::summary()
                    };
                    (m, stat)
                }
                Mark::Rect => {
                    if x.is_none() || y.is_none() {
                        return Err("a heatmap needs a column on both X and Y".to_owned());
                    }
                    if kx == Some(C) {
                        x = binned(&x);
                    }
                    if ky == Some(C) {
                        y = binned(&y);
                    }
                    let numeric_color = color
                        .as_ref()
                        .is_some_and(|f| kind_of(shape, f) == Kind::Continuous);
                    if numeric_color {
                        (
                            m,
                            Stat::Aggregate {
                                function: AggregateFn::Mean,
                                channel: Some(Channel::Color),
                            },
                        )
                    } else if color.is_some() {
                        return Err(
                            "a heatmap colours its cells by a number; take the category off Color"
                                .to_owned(),
                        );
                    } else {
                        (m, Stat::Count)
                    }
                }
                Mark::Mosaic => {
                    if x.is_none() || y.is_none() {
                        return Err("a mosaic needs a column on both X and Y".to_owned());
                    }
                    if kx == Some(C) {
                        x = binned(&x);
                    }
                    if ky == Some(C) {
                        y = binned(&y);
                    }
                    color = None;
                    (m, Stat::Count)
                }
                Mark::Text => {
                    if x.is_none() && y.is_none() {
                        return Err("drop a column on X or Y to draw a plot".to_owned());
                    }
                    if a.label.is_some() && x.is_some() && y.is_some() {
                        (m, Stat::Identity)
                    } else {
                        if kx == Some(C) {
                            x = binned(&x);
                        }
                        if ky == Some(C) {
                            y = binned(&y);
                        }
                        (m, Stat::Count)
                    }
                }
            }
        }
    };

    // A channel that groups rows needs few values: a number there is binned.
    let grouping = stat != Stat::Identity;
    let few = |f: Option<FieldDef>, always: bool| -> Option<FieldDef> {
        f.map(|f| {
            if (always || grouping) && kind_of(shape, &f) == Kind::Continuous && !is_int(shape, &f)
            {
                FieldDef {
                    bin: Some(Default::default()),
                    ..f
                }
            } else {
                f
            }
        })
    };
    let mut layer = Layer::new(mark, stat);
    layer.encoding.x = x;
    layer.encoding.y = y;
    layer.encoding.color = if mark == Mark::Rect {
        color
    } else {
        few(color, false)
    };
    if mark != Mark::Mosaic {
        layer.encoding.size = a.size.clone();
        layer.encoding.shape = few(a.shape.clone(), true);
    }
    layer.encoding.label = a.label.clone();
    let mut spec = PlotSpec::single(data, layer);
    spec.fold = fold;
    spec.coord = coord;
    spec.facet = Facet {
        row: few(a.row.clone(), true),
        column: few(a.column.clone(), true),
        wrap: few(wrap, true),
        ..Facet::default()
    };
    Ok(spec)
}

fn is_int(shape: &StageShape, f: &FieldDef) -> bool {
    shape.column(&f.field).is_some_and(|c| c.ty == ColType::Int)
}

/// The spec for gallery item `preset`, filling the drop zones it needs from
/// `current` where they suit it and from the dataset's columns where not, and
/// the assignment it leaves on the drop zones.
pub fn preset(
    preset: Preset,
    data: DataRef,
    shape: &StageShape,
    current: &Assignment,
) -> Result<(PlotSpec, Assignment), String> {
    if let Some(m) = preset.arrives_in() {
        return Err(format!(
            "{} arrives with milestone {m}",
            preset.label().to_lowercase()
        ));
    }
    let mut a = current.clone();
    let pick = Picker::new(shape);
    let is = |f: &Option<FieldDef>, k: &[Kind]| {
        f.as_ref()
            .is_some_and(|f| shape.column(&f.field).is_some() && k.contains(&kind_of(shape, f)))
    };
    let first_y = a.y.first().cloned();
    let need = |what: &str| format!("{} needs {what}", preset.label().to_lowercase());
    if preset.reshapes() {
        return reshaping(preset, data, shape, a, &pick);
    }
    let mark = match preset {
        Preset::Histogram => {
            if !is(&a.x, &[Kind::Continuous]) {
                a.x = Some(
                    first_y
                        .clone()
                        .filter(|f| is(&Some(f.clone()), &[Kind::Continuous]))
                        .or_else(|| pick.number(&[]))
                        .ok_or_else(|| need("a number column"))?,
                );
            }
            a.y.clear();
            Mark::Bar
        }
        Preset::Bar => {
            if !is(&a.x, &[Kind::Discrete]) {
                a.x = Some(
                    pick.category(&[])
                        .ok_or_else(|| need("a category column"))?,
                );
            }
            a.y.retain(|f| is(&Some(f.clone()), &[Kind::Continuous]));
            Mark::Bar
        }
        Preset::Line | Preset::Area => {
            if !is(&a.x, &[Kind::Temporal, Kind::Continuous]) {
                a.x = Some(
                    pick.date(&[])
                        .or_else(|| pick.number(&[]))
                        .ok_or_else(|| need("a date or number column"))?,
                );
            }
            let x = a.x.as_ref().map(|f| f.field.clone()).unwrap_or_default();
            a.y.retain(|f| is(&Some(f.clone()), &[Kind::Continuous]) && f.field != x);
            if a.y.is_empty() {
                a.y.push(
                    pick.number(&[&x])
                        .ok_or_else(|| need("a number column for Y"))?,
                );
            }
            if preset == Preset::Line {
                Mark::Line
            } else {
                Mark::Area
            }
        }
        Preset::Scatter => {
            if !is(&a.x, &[Kind::Continuous]) {
                let avoid: Vec<String> = a.y.iter().map(|f| f.field.clone()).collect();
                let avoid: Vec<&str> = avoid.iter().map(String::as_str).collect();
                a.x = Some(
                    pick.number(&avoid)
                        .ok_or_else(|| need("two number columns"))?,
                );
            }
            let x = a.x.as_ref().map(|f| f.field.clone()).unwrap_or_default();
            a.y.retain(|f| is(&Some(f.clone()), &[Kind::Continuous]) && f.field != x);
            if a.y.is_empty() {
                a.y.push(
                    pick.number(&[&x])
                        .ok_or_else(|| need("two number columns"))?,
                );
            }
            Mark::Point
        }
        Preset::Box => {
            if !is(&a.x, &[Kind::Discrete]) {
                a.x = Some(
                    pick.category(&[])
                        .ok_or_else(|| need("a category column"))?,
                );
            }
            a.y.retain(|f| is(&Some(f.clone()), &[Kind::Continuous]));
            if a.y.is_empty() {
                a.y.push(pick.number(&[]).ok_or_else(|| need("a number column"))?);
            }
            Mark::Box
        }
        Preset::Heatmap => {
            if !is(&a.x, &[Kind::Discrete, Kind::Temporal]) {
                a.x = Some(
                    pick.category(&[])
                        .ok_or_else(|| need("two category columns"))?,
                );
            }
            let x = a.x.as_ref().map(|f| f.field.clone()).unwrap_or_default();
            a.y.retain(|f| is(&Some(f.clone()), &[Kind::Discrete]) && f.field != x);
            a.y.truncate(1);
            if a.y.is_empty() {
                a.y.push(
                    pick.category(&[&x])
                        .ok_or_else(|| need("two category columns"))?,
                );
            }
            if a.color
                .as_ref()
                .is_some_and(|f| kind_of(shape, f) != Kind::Continuous)
            {
                a.color = None;
            }
            Mark::Rect
        }
        Preset::Map | Preset::Splom | Preset::Parallel | Preset::Correlation | Preset::Mosaic => {
            unreachable!("refused or reshaped above")
        }
    };
    let spec = show_me(data, shape, &a, Some(mark))?;
    Ok((spec, a))
}

/// The most columns a scatterplot matrix, parallel coordinates or a
/// correlation heatmap picks by itself; more can be dropped on Y.
const PICKED_NUMBERS: usize = 4;

/// The presets that reshape the data (A2.11): each builds its spec itself
/// rather than through [`show_me`], over the number columns on Y (or X), or
/// the first few of the dataset's when fewer than two are dropped.
fn reshaping(
    preset: Preset,
    data: DataRef,
    shape: &StageShape,
    mut a: Assignment,
    pick: &Picker,
) -> Result<(PlotSpec, Assignment), String> {
    let need = |what: &str| format!("{} needs {what}", preset.label().to_lowercase());
    let of_kind =
        |f: &FieldDef, k: Kind| shape.column(&f.field).is_some() && kind_of(shape, f) == k;
    if preset == Preset::Mosaic {
        let x =
            a.x.clone()
                .filter(|f| of_kind(f, Kind::Discrete))
                .or_else(|| pick.category(&[]))
                .ok_or_else(|| need("two category columns"))?;
        let y =
            a.y.iter()
                .find(|f| of_kind(f, Kind::Discrete) && f.field != x.field)
                .cloned()
                .or_else(|| pick.category(&[&x.field]))
                .ok_or_else(|| need("two category columns"))?;
        a.x = Some(x.clone());
        a.y = vec![y.clone()];
        a.color = None;
        a.size = None;
        a.shape = None;
        let layer = Layer::new(Mark::Mosaic, Stat::Count)
            .with(Channel::X, x)
            .with(Channel::Y, y);
        let mut spec = PlotSpec::single(data, layer);
        spec.facet = Facet {
            row: a.row.clone(),
            column: a.column.clone(),
            wrap: a.wrap.clone(),
            ..Facet::default()
        };
        return Ok((spec, a));
    }
    // The numbers: those dropped, else the dataset's first few.
    let mut numbers: Vec<String> =
        a.y.iter()
            .chain(a.x.iter())
            .filter(|f| f.bin.is_none() && of_kind(f, Kind::Continuous))
            .map(|f| f.field.clone())
            .collect();
    numbers.dedup();
    if numbers.len() < 2 {
        while numbers.len() < PICKED_NUMBERS {
            let avoid: Vec<&str> = numbers.iter().map(String::as_str).collect();
            match pick.number(&avoid) {
                Some(f) => numbers.push(f.field),
                None => break,
            }
        }
    }
    if numbers.len() < 2 {
        return Err(need("two number columns"));
    }
    a.x = None;
    a.y = numbers.iter().map(FieldDef::of).collect();
    // Color is kept unless it is one of the numbers compared.
    if a.color.as_ref().is_some_and(|c| numbers.contains(&c.field)) {
        a.color = None;
    }
    let color = a.color.clone();
    let pairs_of = |diagonal: bool| Fold::pairs(numbers.clone(), diagonal);
    let spec = match preset {
        Preset::Splom => {
            let fold = pairs_of(false);
            let [kx, vx, ky, vy] = fold.pair_names();
            let mut layer = Layer::new(Mark::Point, Stat::Identity)
                .with(Channel::X, FieldDef::of(vx))
                .with(Channel::Y, FieldDef::of(vy));
            layer.encoding.color = color;
            // A thousand points a plot, so the plots are not starved.
            let plots = (numbers.len() * (numbers.len() - 1)) as u64;
            layer.sample = Some((1_000 * plots).min(MAX_SAMPLE));
            let mut spec = PlotSpec::single(data, layer);
            spec.fold = Some(fold);
            spec.facet = Facet {
                row: Some(FieldDef::of(ky)),
                column: Some(FieldDef::of(kx)),
                scales: FacetScales::Free,
                ..Facet::default()
            };
            spec
        }
        Preset::Parallel => {
            let fold = Fold::of(numbers.clone());
            let mut layer = Layer::new(Mark::Line, Stat::Identity)
                .with(Channel::X, FieldDef::of(fold.key.clone()))
                .with(Channel::Y, FieldDef::of(fold.value.clone()));
            layer.encoding.color = color;
            layer.sample = Some(5_000);
            let mut spec = PlotSpec::single(data, layer);
            spec.fold = Some(fold);
            spec.coord = Coord::Parallel;
            spec
        }
        Preset::Correlation => {
            let fold = pairs_of(true);
            let [kx, vx, ky, vy] = fold.pair_names();
            let layer = |mark: Mark| {
                Layer::new(
                    mark,
                    Stat::Correlation {
                        x: vx.clone(),
                        y: vy.clone(),
                    },
                )
                .with(Channel::X, FieldDef::of(kx.clone()))
                .with(Channel::Y, FieldDef::of(ky.clone()))
            };
            let mut spec = PlotSpec::single(data, layer(Mark::Rect));
            spec.layers.push(layer(Mark::Text));
            spec.fold = Some(fold);
            spec.scales.insert(
                Channel::Color,
                Scale {
                    domain: Some(vec![(-1).into(), 1.into()]),
                    scheme: Some("diverging".to_owned()),
                    ..Scale::default()
                },
            );
            a.color = None;
            spec
        }
        Preset::Histogram
        | Preset::Bar
        | Preset::Line
        | Preset::Scatter
        | Preset::Box
        | Preset::Heatmap
        | Preset::Area
        | Preset::Mosaic
        | Preset::Map => unreachable!("not a reshaping preset"),
    };
    Ok((spec, a))
}

/// Picks columns for a preset: never the row key, and a foreign key only as
/// a category.
struct Picker<'a> {
    shape: &'a StageShape,
    key: Option<&'a str>,
}

impl<'a> Picker<'a> {
    fn new(shape: &'a StageShape) -> Picker<'a> {
        let key = match &shape.grain {
            Grain::Table { key, .. } => Some(key.as_str()),
            _ => None,
        };
        Picker { shape, key }
    }

    fn find(
        &self,
        avoid: &[&str],
        want: impl Fn(&sc_dataset::StageColumn) -> bool,
    ) -> Option<FieldDef> {
        self.shape
            .columns
            .iter()
            .filter(|c| Some(c.name.as_str()) != self.key && !avoid.contains(&c.name.as_str()))
            .find(|c| want(c))
            .map(|c| FieldDef::of(c.name.clone()))
    }

    fn number(&self, avoid: &[&str]) -> Option<FieldDef> {
        self.find(avoid, |c| c.key.is_none() && c.ty.is_numeric())
    }

    fn category(&self, avoid: &[&str]) -> Option<FieldDef> {
        self.find(avoid, |c| {
            c.key.is_some() || matches!(c.ty, ColType::Text | ColType::Bool)
        })
    }

    fn date(&self, avoid: &[&str]) -> Option<FieldDef> {
        self.find(avoid, |c| {
            matches!(c.ty, ColType::Date | ColType::Timestamp)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plot::validate::validate;
    use sc_dataset::{DatasetId, ForeignKey, StageColumn};

    fn shape() -> StageShape {
        let col = |name: &str, ty: ColType| StageColumn {
            name: name.into(),
            ty,
            key: None,
        };
        StageShape {
            columns: vec![
                col("id", ColType::Int),
                col("price", ColType::Float),
                col("area", ColType::Float),
                StageColumn {
                    name: "neighbourhood".into(),
                    ty: ColType::Int,
                    key: Some(ForeignKey {
                        table: "neighbourhoods".into(),
                        field: "id".into(),
                    }),
                },
                col("year_built", ColType::Int),
                col("sold", ColType::Bool),
                col("listed_on", ColType::Date),
            ],
            grain: Grain::Table {
                table: "houses".into(),
                key: "id".into(),
            },
        }
    }

    fn data() -> DataRef {
        DataRef::Dataset {
            dataset: DatasetId::new(),
        }
    }

    fn on(x: Option<&str>, y: &[&str]) -> Assignment {
        Assignment {
            x: x.map(FieldDef::of),
            y: y.iter().map(|f| FieldDef::of(*f)).collect(),
            ..Assignment::default()
        }
    }

    fn chosen(a: &Assignment, mark: Option<Mark>) -> (Mark, Stat, Coord) {
        let spec = show_me(data(), &shape(), a, mark).unwrap();
        assert!(
            validate(&spec, &shape()).is_empty(),
            "{:?}",
            validate(&spec, &shape())
        );
        (spec.layers[0].mark, spec.layers[0].stat.clone(), spec.coord)
    }

    #[test]
    fn the_mark_follows_the_column_types() {
        use Coord::{Cartesian, Flipped};
        let cases: Vec<(Assignment, Mark, Stat, Coord)> = vec![
            (on(Some("price"), &[]), Mark::Bar, Stat::Count, Cartesian),
            (
                on(Some("neighbourhood"), &[]),
                Mark::Bar,
                Stat::Count,
                Cartesian,
            ),
            (
                on(Some("listed_on"), &[]),
                Mark::Line,
                Stat::Count,
                Cartesian,
            ),
            (on(None, &["price"]), Mark::Bar, Stat::Count, Cartesian),
            (
                on(Some("area"), &["price"]),
                Mark::Point,
                Stat::Identity,
                Cartesian,
            ),
            (
                on(Some("neighbourhood"), &["price"]),
                Mark::Box,
                Stat::boxplot(),
                Cartesian,
            ),
            (
                on(Some("price"), &["sold"]),
                Mark::Box,
                Stat::boxplot(),
                Flipped,
            ),
            (
                on(Some("listed_on"), &["price"]),
                Mark::Line,
                Stat::aggregate(AggregateFn::Mean),
                Cartesian,
            ),
            (
                on(Some("neighbourhood"), &["sold"]),
                Mark::Rect,
                Stat::Count,
                Cartesian,
            ),
        ];
        for (a, mark, stat, coord) in cases {
            assert_eq!(chosen(&a, None), (mark, stat, coord), "{a:?}");
        }
        // A number alone on X is binned; alone on Y it is binned and counted
        // across.
        let hist = show_me(data(), &shape(), &on(Some("price"), &[]), None).unwrap();
        assert!(hist.layers[0].encoding.x.as_ref().unwrap().bin.is_some());
        // A flipped box plot puts the category on X and the number on Y.
        let flipped = show_me(data(), &shape(), &on(Some("price"), &["sold"]), None).unwrap();
        assert_eq!(flipped.layers[0].encoding.x.as_ref().unwrap().field, "sold");
        assert_eq!(
            flipped.layers[0].encoding.y.as_ref().unwrap().field,
            "price"
        );
        assert_eq!(
            show_me(data(), &shape(), &Assignment::default(), None).unwrap_err(),
            "drop a column on X or Y to draw a plot"
        );
    }

    #[test]
    fn the_mark_palette_overrides_the_choice() {
        let a = on(Some("neighbourhood"), &["price"]);
        assert_eq!(
            chosen(&a, Some(Mark::Bar)).1,
            Stat::aggregate(AggregateFn::Mean)
        );
        assert_eq!(chosen(&a, Some(Mark::Point)).1, Stat::Identity);
        assert_eq!(chosen(&a, Some(Mark::Errorbar)).1, Stat::summary());
        let scatter = on(Some("area"), &["price"]);
        assert_eq!(chosen(&scatter, Some(Mark::Line)).1, Stat::Identity);
        let (_, stat, _) = chosen(&scatter, Some(Mark::Box));
        assert_eq!(stat, Stat::boxplot());
        let heat = show_me(data(), &shape(), &scatter, Some(Mark::Rect)).unwrap();
        assert!(heat.layers[0].encoding.x.as_ref().unwrap().bin.is_some());
        assert!(heat.layers[0].encoding.y.as_ref().unwrap().bin.is_some());
        assert_eq!(
            show_me(
                data(),
                &shape(),
                &on(Some("neighbourhood"), &["sold"]),
                Some(Mark::Box)
            )
            .unwrap_err(),
            "box plots need a number on Y"
        );
    }

    #[test]
    fn several_columns_on_y_are_one_variable_coloured_by_column() {
        let a = Assignment {
            x: Some(FieldDef::of("year_built")),
            y: vec![FieldDef::of("price"), FieldDef::of("area")],
            ..Assignment::default()
        };
        let spec = show_me(data(), &shape(), &a, Some(Mark::Line)).unwrap();
        assert_eq!(spec.fold.as_ref().unwrap().columns, vec!["price", "area"]);
        assert_eq!(spec.layers[0].encoding.y.as_ref().unwrap().field, "value");
        assert_eq!(
            spec.layers[0].encoding.color.as_ref().unwrap().field,
            "variable"
        );
        assert!(validate(&spec, &shape()).is_empty());
    }

    #[test]
    fn a_grouping_channel_bins_a_number() {
        let a = Assignment {
            x: Some(FieldDef::of("neighbourhood")),
            y: vec![FieldDef::of("price")],
            color: Some(FieldDef::of("area")),
            wrap: Some(FieldDef::of("year_built")),
            ..Assignment::default()
        };
        let spec = show_me(data(), &shape(), &a, None).unwrap();
        assert!(
            spec.layers[0]
                .encoding
                .color
                .as_ref()
                .unwrap()
                .bin
                .is_some()
        );
        // An integer is a fine facet as it is.
        assert!(spec.facet.wrap.as_ref().unwrap().bin.is_none());
        assert!(validate(&spec, &shape()).is_empty());
    }

    #[test]
    fn presets_fill_the_drop_zones_they_need() {
        let s = shape();
        for p in Preset::ALL {
            let made = preset(p, data(), &s, &Assignment::default());
            if p == Preset::Map {
                assert_eq!(made.unwrap_err(), "map arrives with milestone A5");
                continue;
            }
            let (spec, _) = made.unwrap_or_else(|e| panic!("{p:?}: {e}"));
            assert!(
                validate(&spec, &s).is_empty(),
                "{p:?}: {:?}",
                validate(&spec, &s)
            );
        }
        let (hist, a) = preset(Preset::Histogram, data(), &s, &Assignment::default()).unwrap();
        // The row key is never picked: the first number is `price`.
        assert_eq!(a.x.unwrap().field, "price");
        assert_eq!(hist.layers[0].stat, Stat::Count);
        let (scatter, a) = preset(Preset::Scatter, data(), &s, &Assignment::default()).unwrap();
        assert_eq!(
            (a.x.unwrap().field, a.y[0].field.clone()),
            ("price".to_owned(), "area".to_owned())
        );
        assert_eq!(scatter.layers[0].mark, Mark::Point);
        // What is already dropped is kept where it suits the preset.
        let (_, a) = preset(Preset::Box, data(), &s, &on(Some("sold"), &["area"])).unwrap();
        assert_eq!(
            (a.x.unwrap().field, a.y[0].field.clone()),
            ("sold".to_owned(), "area".to_owned())
        );
        let (line, _) = preset(Preset::Line, data(), &s, &Assignment::default()).unwrap();
        assert_eq!(
            line.layers[0].encoding.x.as_ref().unwrap().field,
            "listed_on"
        );
        let items = gallery();
        assert_eq!(items.len(), 12);
        assert!(
            !items
                .iter()
                .find(|i| i.preset == Preset::Map)
                .unwrap()
                .available
        );
    }
}
