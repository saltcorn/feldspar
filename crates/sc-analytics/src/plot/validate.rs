//! Checking a spec against a dataset (analytics TODO A2.1): the columns exist,
//! their types suit their channels and the layer's stat, and the marks suit the
//! stats — each refusal a sentence, all of them at once, so the explorer can
//! say everything that is wrong with what was dropped.
//!
//! The same walk makes each layer's **plan**: which channels group the rows
//! (the dimensions), which columns the stat reads (its inputs), and which
//! channel a count or a summary is drawn on. [`render_plot`](super::render_plot)
//! compiles the plan, so a spec that validates is one that renders.

use std::collections::BTreeSet;

use sc_dataset::{ColType, Grain, StageColumn, StageShape};

use super::spec::{
    AggregateFn, Bin, Cell, Channel, Coord, Facet, FieldDef, Fold, Layer, Mark, PlotSpec,
    ScaleKind, SelectionKind, Stat, TableSpec,
};

/// A channel that groups a layer's rows, or — for a layer that draws rows —
/// one of the columns it draws.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dim {
    pub channel: Channel,
    pub field: String,
    pub ty: ColType,
    pub bin: Option<Bin>,
}

/// A column a stat reads: what a summary summarises, a box plot's Y, a
/// density's X, a smoother's X and Y.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Input {
    pub channel: Channel,
    pub field: String,
}

/// What a layer computes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayerPlan {
    pub dims: Vec<Dim>,
    pub inputs: Vec<Input>,
    /// The channel a count or an aggregate is drawn on.
    pub out: Option<Channel>,
}

/// The columns a spec's layers read: the dataset's last stage, with the fold
/// applied — the folded columns replaced by the key and the value.
pub fn folded_shape(shape: &StageShape, fold: Option<&Fold>) -> Result<StageShape, Vec<String>> {
    let Some(fold) = fold else {
        return Ok(shape.clone());
    };
    let mut problems = Vec::new();
    if fold.columns.len() < 2 {
        problems.push("comparing columns as one variable needs at least two columns".to_owned());
    }
    let mut seen = BTreeSet::new();
    for name in &fold.columns {
        if !seen.insert(name.as_str()) {
            problems.push(format!("`{name}` is compared with itself"));
            continue;
        }
        match shape.column(name) {
            None => problems.push(missing(shape, name)),
            Some(c) if !c.ty.is_numeric() && c.ty != ColType::Unknown => problems.push(format!(
                "`{name}` is {}, and only numbers can be compared as one variable",
                article(c.ty)
            )),
            Some(_) => {}
        }
    }
    let (key, value) = (fold.key.trim(), fold.value.trim());
    if key.is_empty() || value.is_empty() {
        problems.push("the columns a comparison makes need names".to_owned());
    } else if key == value {
        problems.push(format!(
            "the comparison's two new columns are both called `{key}`"
        ));
    }
    let made: Vec<String> = match fold.pairs {
        None => vec![key.to_owned(), value.to_owned()],
        Some(_) => fold.pair_names().to_vec(),
    };
    for name in &made {
        if !key.is_empty()
            && !value.is_empty()
            && !fold.columns.iter().any(|c| c == name)
            && shape.column(name).is_some()
        {
            problems.push(format!(
                "comparing columns makes a column `{name}`, and the dataset already has one; \
                 give the new column another name"
            ));
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut columns: Vec<StageColumn> = shape
        .columns
        .iter()
        .filter(|c| !fold.columns.contains(&c.name))
        .cloned()
        .collect();
    // The names are text and the values numbers, alternately.
    for (i, name) in made.into_iter().enumerate() {
        columns.push(StageColumn {
            name,
            ty: if i % 2 == 0 {
                ColType::Text
            } else {
                ColType::Float
            },
            key: None,
        });
    }
    Ok(StageShape {
        columns,
        grain: Grain::Derived,
    })
}

/// Every reason `spec` cannot be drawn over a dataset whose last stage is
/// `shape`; empty when it can.
pub fn validate(spec: &PlotSpec, shape: &StageShape) -> Vec<String> {
    let shape = match folded_shape(shape, spec.fold.as_ref()) {
        Ok(s) => s,
        Err(problems) => return problems,
    };
    let mut problems = Vec::new();
    if spec.layers.is_empty() {
        problems.push("a plot needs at least one layer".to_owned());
    }
    problems.extend(facet_problems(&spec.facet, &shape));
    let many = spec.layers.len() > 1;
    let mut plans = Vec::with_capacity(spec.layers.len());
    for (i, layer) in spec.layers.iter().enumerate() {
        match plan(layer, &spec.facet, &shape) {
            Ok(p) => plans.push(Some(p)),
            Err(errs) => {
                plans.push(None);
                problems.extend(errs.into_iter().map(|e| {
                    if many {
                        format!("Layer {} ({}): {e}", i + 1, layer.mark.describe())
                    } else {
                        e
                    }
                }));
            }
        }
        if spec.coord == Coord::Polar
            && matches!(
                layer.mark,
                Mark::Box | Mark::Band | Mark::Errorbar | Mark::Rect | Mark::Mosaic
            )
        {
            problems.push(format!(
                "a polar plot cannot draw {}",
                layer.mark.describe()
            ));
        }
    }
    if spec.coord == Coord::Parallel {
        problems.extend(parallel_problems(spec));
    }
    // Scales: only the channels that have one, and a log or square-root scale
    // only over numbers.
    for (channel, scale) in &spec.scales {
        if !matches!(
            channel,
            Channel::X | Channel::Y | Channel::Color | Channel::Size
        ) {
            problems.push(format!("{} has no scale", channel.describe()));
            continue;
        }
        if scale.kind != ScaleKind::Linear {
            for (layer, plan) in spec.layers.iter().zip(&plans) {
                let Some(plan) = plan else { continue };
                if plan.out == Some(*channel) || drawn_by_stat(&layer.stat, *channel) {
                    continue;
                }
                if let Some(f) = layer.encoding.get(*channel)
                    && let Some(c) = shape.column(&f.field)
                    && !c.ty.is_numeric()
                    && c.ty != ColType::Unknown
                {
                    problems.push(format!(
                        "a {} scale on {} needs numbers, and `{}` is {}",
                        if scale.kind == ScaleKind::Log {
                            "log"
                        } else {
                            "square-root"
                        },
                        channel.describe(),
                        f.field,
                        article(c.ty)
                    ));
                }
            }
        }
        if let Some(domain) = &scale.domain
            && domain.is_empty()
        {
            problems.push(format!(
                "the scale on {} has an empty domain",
                channel.describe()
            ));
        }
    }
    for r in &spec.references {
        if !r.channel.is_positional() {
            problems.push("a reference line is drawn at a value of X or of Y".to_owned());
        } else if !(r.value.is_number() || r.value.is_string()) {
            problems.push(format!(
                "a reference line on {} is at a number, or a value as the axis shows it",
                r.channel.describe()
            ));
        }
    }
    let mut names = BTreeSet::new();
    for s in &spec.selections {
        let name = s.name.trim();
        if name.is_empty() {
            problems.push("a selection needs a name".to_owned());
        } else if !names.insert(name.to_owned()) {
            problems.push(format!("two selections are called `{name}`"));
        }
        if s.channels.is_empty() {
            problems.push(format!("the selection `{name}` filters on no channel"));
        }
        for c in &s.channels {
            if s.kind == SelectionKind::Interval && !c.is_positional() {
                problems.push(format!(
                    "the selection `{name}` brushes a range, which is along X or Y, not {}",
                    c.describe()
                ));
            }
            let used = spec.facet.get(*c).is_some()
                || spec.layers.iter().any(|l| l.encoding.get(*c).is_some());
            if !used {
                problems.push(format!(
                    "the selection `{name}` filters on {}, which shows no column",
                    c.describe()
                ));
            }
        }
    }
    problems
}

/// What parallel coordinates need: the columns compared as one variable, and
/// each layer a line of the rows across them.
fn parallel_problems(spec: &PlotSpec) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(fold) = spec.fold.as_ref().filter(|f| f.pairs.is_none()) else {
        return vec![
            "parallel coordinates draw several columns compared as one variable; put at least two number columns on Y"
                .to_owned(),
        ];
    };
    for layer in &spec.layers {
        if layer.mark != Mark::Line || layer.stat != Stat::Identity {
            problems.push(format!(
                "parallel coordinates draw each row as a line, not {}",
                if layer.stat == Stat::Identity {
                    layer.mark.describe()
                } else {
                    layer.stat.describe()
                }
            ));
        }
        let on = |c: Channel| layer.encoding.get(c).map(|f| f.field.as_str());
        if on(Channel::X) != Some(fold.key.trim()) || on(Channel::Y) != Some(fold.value.trim()) {
            problems.push(format!(
                "parallel coordinates put the compared columns' names (`{}`) on X and their values (`{}`) on Y",
                fold.key.trim(),
                fold.value.trim()
            ));
        }
    }
    if !spec.facet.is_empty() {
        problems.push("parallel coordinates are not split into small multiples".to_owned());
    }
    if !spec.references.is_empty() {
        problems.push("parallel coordinates have no reference lines".to_owned());
    }
    if spec
        .scales
        .iter()
        .any(|(c, s)| c.is_positional() && s.kind != ScaleKind::Linear)
    {
        problems.push("parallel coordinates have an axis per column, each linear".to_owned());
    }
    problems
}

/// Whether a layer's stat computes `channel` itself rather than reading it
/// from a column: a density's Y.
fn drawn_by_stat(stat: &Stat, channel: Channel) -> bool {
    matches!(stat, Stat::Density { .. }) && channel == Channel::Y
}

fn facet_problems(facet: &Facet, shape: &StageShape) -> Vec<String> {
    let mut problems = Vec::new();
    if facet.wrap.is_some() && (facet.row.is_some() || facet.column.is_some()) {
        problems.push("Wrap cannot be combined with Facet rows or Facet columns".to_owned());
    }
    if facet.columns == Some(0) {
        problems.push("a wrapped facet needs at least one plot in a row".to_owned());
    }
    for (channel, f) in facet.iter() {
        if let Err(e) = field_check(channel, f, shape, true) {
            problems.push(e);
        }
    }
    problems
}

/// The plan of one layer, or every reason it cannot be drawn.
pub(crate) fn plan(
    layer: &Layer,
    facet: &Facet,
    shape: &StageShape,
) -> Result<LayerPlan, Vec<String>> {
    let mut problems = Vec::new();
    let enc = &layer.encoding;
    let grouping = layer.stat != Stat::Identity;
    // Check every column and collect its type.
    let mut typed: Vec<(Channel, &FieldDef, ColType)> = Vec::new();
    for (channel, f) in enc.iter() {
        match field_check(channel, f, shape, grouping) {
            Ok(ty) => typed.push((channel, f, ty)),
            Err(e) => problems.push(e),
        }
    }
    let ty_of = |c: Channel| typed.iter().find(|(ch, _, _)| *ch == c).map(|(_, _, t)| *t);
    let has = |c: Channel| enc.get(c).is_some();
    let mark = layer.mark;
    let allowed = |marks: &[Mark]| marks.contains(&mark);
    let refuse_mark = |problems: &mut Vec<String>, what: &str| {
        problems.push(format!("{what} cannot be drawn as {}", mark.describe()));
    };
    let numeric_input = |problems: &mut Vec<String>, c: Channel, why: &str| match enc.get(c) {
        None => problems.push(format!("{why} needs a column on {}", c.describe())),
        Some(f) => {
            if f.bin.is_some() {
                problems.push(format!(
                    "{why} reads `{}` as it is, so it is not binned",
                    f.field
                ));
            }
            if let Some(t) = ty_of(c)
                && !t.is_numeric()
                && t != ColType::Unknown
            {
                problems.push(format!(
                    "{why} needs numbers on {}, and `{}` is {}",
                    c.describe(),
                    f.field,
                    article(t)
                ));
            }
        }
    };
    let mut inputs = Vec::new();
    let mut out = None;
    let input = |c: Channel| Input {
        channel: c,
        field: enc.get(c).map(|f| f.field.clone()).unwrap_or_default(),
    };
    match &layer.stat {
        Stat::Identity if mark == Mark::Mosaic => {
            problems.push(
                "a mosaic is drawn from counts, so the layer needs the count stat".to_owned(),
            );
        }
        Stat::Identity => {
            if matches!(mark, Mark::Box | Mark::Band | Mark::Errorbar) {
                problems.push(format!(
                    "{} are drawn from a summary, so the layer needs {}",
                    capitalise(mark.describe()),
                    if mark == Mark::Box {
                        "the box plot stat"
                    } else {
                        "a summary or a smoother"
                    }
                ));
            }
            if !has(Channel::X) && !has(Channel::Y) {
                problems.push("drop a column on X or Y to draw the rows".to_owned());
            } else if matches!(mark, Mark::Line | Mark::Area | Mark::Bar | Mark::Rect)
                && !(has(Channel::X) && has(Channel::Y))
            {
                problems.push(format!(
                    "{} needs a column on both X and Y",
                    capitalise(mark.describe())
                ));
            }
            if mark == Mark::Text && !has(Channel::Label) {
                problems.push("text needs a column on Label".to_owned());
            }
        }
        Stat::Count if mark == Mark::Mosaic => {
            // The count is the tile's area: drawn on Size, which nothing else
            // may show.
            if !(has(Channel::X) && has(Channel::Y)) {
                problems.push("a mosaic needs a column on both X and Y".to_owned());
            }
            for c in [Channel::X, Channel::Y] {
                if let (Some(f), Some(ColType::Float | ColType::Decimal)) = (enc.get(c), ty_of(c))
                    && f.bin.is_none()
                {
                    problems.push(format!(
                        "a mosaic's tiles are the values of `{}`, a number with many values; bin it",
                        f.field
                    ));
                }
            }
            for c in [Channel::Color, Channel::Size, Channel::Shape] {
                if let Some(f) = enc.get(c) {
                    problems.push(format!(
                        "a mosaic colours its tiles by Y and sizes them by the count, so `{}` cannot be on {}",
                        f.field,
                        c.describe()
                    ));
                }
            }
            out = Some(Channel::Size);
        }
        Stat::Count => {
            if !allowed(&[
                Mark::Point,
                Mark::Line,
                Mark::Bar,
                Mark::Area,
                Mark::Text,
                Mark::Rect,
            ]) {
                refuse_mark(&mut problems, "a count");
            }
            if !has(Channel::X) && !has(Channel::Y) {
                problems.push("a count needs a column on X or Y to count by".to_owned());
            }
            out = if mark == Mark::Text && !has(Channel::Label) {
                Some(Channel::Label)
            } else {
                [Channel::Y, Channel::X, Channel::Color]
                    .into_iter()
                    .find(|c| !has(*c))
            };
            if out.is_none() {
                problems.push(
                    "X, Y and Color all show columns, so there is nowhere to draw the count"
                        .to_owned(),
                );
            }
            if mark == Mark::Rect && !(has(Channel::X) && has(Channel::Y)) {
                problems.push("a heatmap needs a column on both X and Y".to_owned());
            }
        }
        Stat::Aggregate { function, channel } => {
            let ch = channel.unwrap_or(Channel::Y);
            if !allowed(&[
                Mark::Point,
                Mark::Line,
                Mark::Bar,
                Mark::Area,
                Mark::Text,
                Mark::Rect,
            ]) {
                refuse_mark(&mut problems, "a summary");
            }
            if ch.is_facet() {
                problems.push(format!(
                    "a summary is drawn on a channel, not on {}",
                    ch.describe()
                ));
            } else {
                match enc.get(ch) {
                    None => problems.push(format!(
                        "the {} needs a column on {} to summarise",
                        function.describe(),
                        ch.describe()
                    )),
                    Some(f) => {
                        if f.bin.is_some() {
                            problems.push(format!(
                                "the {} reads `{}` as it is, so it is not binned",
                                function.describe(),
                                f.field
                            ));
                        }
                        if let Some(t) = ty_of(ch) {
                            let numeric = matches!(
                                function,
                                AggregateFn::Sum
                                    | AggregateFn::Mean
                                    | AggregateFn::Median
                                    | AggregateFn::Sd
                            );
                            if numeric && !t.is_numeric() && t != ColType::Unknown {
                                problems.push(format!(
                                    "the {} needs numbers, and `{}` is {}",
                                    function.describe(),
                                    f.field,
                                    article(t)
                                ));
                            }
                            if matches!(function, AggregateFn::Min | AggregateFn::Max)
                                && !t.is_ordered()
                                && t != ColType::Unknown
                            {
                                problems.push(format!(
                                    "`{}` is {}, which has no smallest or largest value",
                                    f.field,
                                    article(t)
                                ));
                            }
                        }
                        inputs.push(input(ch));
                        out = Some(ch);
                    }
                }
            }
            if mark == Mark::Rect && !(has(Channel::X) && has(Channel::Y)) {
                problems.push("a heatmap needs a column on both X and Y".to_owned());
            }
        }
        Stat::Quantiles { probabilities } => {
            if !allowed(&[Mark::Point, Mark::Line]) {
                refuse_mark(&mut problems, "quantiles");
            }
            if probabilities.is_empty() {
                problems.push("quantiles need at least one probability".to_owned());
            }
            if probabilities.iter().any(|p| !(0.0..=1.0).contains(p)) {
                problems.push("a quantile's probability is between 0 and 1".to_owned());
            }
            numeric_input(&mut problems, Channel::Y, "quantiles");
            inputs.push(input(Channel::Y));
        }
        Stat::Boxplot { coef } => {
            if mark != Mark::Box {
                refuse_mark(&mut problems, "a box plot");
            }
            if *coef <= 0.0 {
                problems.push(
                    "a box plot's whiskers reach more than 0 interquartile ranges".to_owned(),
                );
            }
            numeric_input(&mut problems, Channel::Y, "a box plot");
            inputs.push(input(Channel::Y));
        }
        Stat::Summary { level } => {
            if !allowed(&[
                Mark::Point,
                Mark::Bar,
                Mark::Line,
                Mark::Errorbar,
                Mark::Band,
            ]) {
                refuse_mark(&mut problems, "a mean with a confidence interval");
            }
            check_level(&mut problems, *level);
            numeric_input(&mut problems, Channel::Y, "a mean");
            inputs.push(input(Channel::Y));
        }
        Stat::Density { bandwidth, adjust } => {
            if !allowed(&[Mark::Line, Mark::Area]) {
                refuse_mark(&mut problems, "a density");
            }
            if bandwidth.is_some_and(|b| b <= 0.0) || *adjust <= 0.0 {
                problems.push("a density's bandwidth is more than 0".to_owned());
            }
            if let Some(f) = enc.get(Channel::Y) {
                problems.push(format!(
                    "a density draws its own Y, so `{}` cannot be on Y",
                    f.field
                ));
            }
            numeric_input(&mut problems, Channel::X, "a density");
            inputs.push(input(Channel::X));
        }
        Stat::Smooth { span, level, .. } => {
            if !allowed(&[Mark::Line, Mark::Band]) {
                refuse_mark(&mut problems, "a smoother");
            }
            if *span <= 0.0 {
                problems.push("a loess span is more than 0".to_owned());
            }
            check_level(&mut problems, *level);
            numeric_input(&mut problems, Channel::X, "a smoother");
            numeric_input(&mut problems, Channel::Y, "a smoother");
            inputs.push(input(Channel::X));
            inputs.push(input(Channel::Y));
        }
        Stat::Correlation { x, y } => {
            if !allowed(&[Mark::Rect, Mark::Text, Mark::Point]) {
                refuse_mark(&mut problems, "a correlation");
            }
            for name in [x, y] {
                match shape.column(name) {
                    None => problems.push(format!("a correlation: {}", missing(shape, name))),
                    Some(c) if !c.ty.is_numeric() && c.ty != ColType::Unknown => {
                        problems.push(format!(
                            "a correlation needs numbers, and `{name}` is {}",
                            article(c.ty)
                        ))
                    }
                    Some(_) => {}
                }
            }
            out = if mark == Mark::Text && !has(Channel::Label) {
                Some(Channel::Label)
            } else if !has(Channel::Color) {
                Some(Channel::Color)
            } else {
                problems.push(
                    "Color shows a column, so there is nowhere to draw the correlation".to_owned(),
                );
                None
            };
        }
    }
    if mark == Mark::Box
        && !matches!(layer.stat, Stat::Boxplot { .. })
        && layer.stat != Stat::Identity
    {
        problems.push("box plots need the box plot stat".to_owned());
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let input_channels: Vec<Channel> = inputs.iter().map(|i| i.channel).collect();
    let mut dims: Vec<Dim> = typed
        .into_iter()
        .filter(|(c, _, _)| !input_channels.contains(c))
        .map(|(channel, f, ty)| Dim {
            channel,
            field: f.field.clone(),
            ty,
            bin: f.bin,
        })
        .collect();
    for (channel, f) in facet.iter() {
        if let Some(c) = shape.column(&f.field) {
            dims.push(Dim {
                channel,
                field: f.field.clone(),
                ty: c.ty,
                bin: f.bin,
            });
        }
    }
    Ok(LayerPlan { dims, inputs, out })
}

/// Every reason a summary table cannot be made over a dataset whose last
/// stage is `shape`; empty when it can.
pub fn validate_table(spec: &TableSpec, shape: &StageShape) -> Vec<String> {
    let shape = match folded_shape(shape, spec.fold.as_ref()) {
        Ok(s) => s,
        Err(problems) => return problems,
    };
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (side, f) in spec
        .rows
        .iter()
        .map(|f| ("rows", f))
        .chain(spec.columns.iter().map(|f| ("columns", f)))
    {
        if !seen.insert(f.field.as_str()) {
            problems.push(format!("`{}` is used twice as rows or columns", f.field));
            continue;
        }
        match field_check(Channel::X, f, &shape, true) {
            Err(e) => problems.push(e.trim_start_matches("X: ").to_owned()),
            Ok(ty) => {
                if matches!(ty, ColType::Float | ColType::Decimal) && f.bin.is_none() {
                    problems.push(format!(
                        "the table's {side} are the values of `{}`, a number with many values; bin it",
                        f.field
                    ));
                }
            }
        }
    }
    for cell in &spec.cells {
        problems.extend(cell_problems(cell, &shape));
    }
    problems
}

fn cell_problems(cell: &Cell, shape: &StageShape) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(name) = &cell.field else {
        if cell.function != AggregateFn::Count {
            problems.push(format!(
                "the {} needs a column to summarise",
                cell.function.describe()
            ));
        }
        return problems;
    };
    let Some(column) = shape.column(name) else {
        problems.push(format!("a cell: {}", missing(shape, name)));
        return problems;
    };
    let ty = column.ty;
    let numeric = matches!(
        cell.function,
        AggregateFn::Sum | AggregateFn::Mean | AggregateFn::Median | AggregateFn::Sd
    );
    if numeric && !ty.is_numeric() && ty != ColType::Unknown {
        problems.push(format!(
            "the {} needs numbers, and `{name}` is {}",
            cell.function.describe(),
            article(ty)
        ));
    }
    if matches!(cell.function, AggregateFn::Min | AggregateFn::Max)
        && !ty.is_ordered()
        && ty != ColType::Unknown
    {
        problems.push(format!(
            "`{name}` is {}, which has no smallest or largest value",
            article(ty)
        ));
    }
    problems
}

fn check_level(problems: &mut Vec<String>, level: f64) {
    if !(level > 0.0 && level < 1.0) {
        problems.push("a confidence level is between 0 and 1, such as 0.95".to_owned());
    }
}

/// Check one column on one channel and answer its type. `grouping` is whether
/// the layer groups its rows by the channel (any stat but the identity, and
/// every facet).
pub(crate) fn field_check(
    channel: Channel,
    f: &FieldDef,
    shape: &StageShape,
    grouping: bool,
) -> Result<ColType, String> {
    let Some(column) = shape.column(&f.field) else {
        return Err(format!(
            "{}: {}",
            channel.describe(),
            missing(shape, &f.field)
        ));
    };
    let ty = column.ty;
    let name = &f.field;
    if matches!(ty, ColType::Json | ColType::Bytes) {
        return Err(format!(
            "`{name}` is {}, which cannot be plotted",
            article(ty)
        ));
    }
    if ty == ColType::Geometry {
        return Err(format!(
            "`{name}` is a geometry, which is drawn on a map rather than plotted"
        ));
    }
    if let Some(bin) = &f.bin {
        if !ty.is_numeric() && ty != ColType::Unknown {
            return Err(format!(
                "`{name}` is {}, and only numbers can be binned",
                article(ty)
            ));
        }
        if bin.width.is_some_and(|w| w <= 0.0) {
            return Err(format!("the bins of `{name}` need a width of more than 0"));
        }
        if bin.bins == Some(0) {
            return Err(format!("`{name}` needs at least one bin"));
        }
    }
    let continuous = matches!(ty, ColType::Float | ColType::Decimal) && f.bin.is_none();
    match channel {
        Channel::Size if !ty.is_numeric() && ty != ColType::Unknown => Err(format!(
            "Size shows an amount, and `{name}` is {}; put it on Color or Shape",
            article(ty)
        )),
        Channel::Shape if continuous => Err(format!(
            "Shape needs a column with a few values, and `{name}` is a number with many; bin it, or put it on Color"
        )),
        c if c.is_facet() && continuous => Err(format!(
            "{} makes one plot per value, and `{name}` is a number with many values; bin it",
            c.describe()
        )),
        c if grouping && continuous && !c.is_positional() && !c.is_facet() => Err(format!(
            "{} groups the rows here, and `{name}` is a number with many values; bin it",
            c.describe()
        )),
        _ => Ok(ty),
    }
}

/// The sentence for a column that is not there.
fn missing(shape: &StageShape, name: &str) -> String {
    let names: Vec<String> = shape
        .columns
        .iter()
        .map(|c| format!("`{}`", c.name))
        .collect();
    format!(
        "`{name}` is not a column of the dataset (its columns are {})",
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    )
}

/// A type's name with its article: "a number", "text", "an integer".
pub(crate) fn article(ty: ColType) -> String {
    match ty {
        ColType::Text | ColType::Json | ColType::Bytes => ty.name().to_owned(),
        ColType::Int | ColType::Unknown => format!("an {}", ty.name()),
        _ => format!("a {}", ty.name()),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plot::spec::{DataRef, Reference, Scale, Selection};
    use sc_dataset::DatasetId;
    use serde_json::json;

    fn col(name: &str, ty: ColType) -> StageColumn {
        StageColumn {
            name: name.into(),
            ty,
            key: None,
        }
    }

    fn houses() -> StageShape {
        StageShape {
            columns: vec![
                col("id", ColType::Int),
                col("price", ColType::Float),
                col("area", ColType::Float),
                col("neighbourhood", ColType::Int),
                col("year_built", ColType::Int),
                col("street", ColType::Text),
                col("sold", ColType::Bool),
                col("built_on", ColType::Date),
                col("extra", ColType::Json),
            ],
            grain: Grain::Table {
                table: "houses".into(),
                key: "id".into(),
            },
        }
    }

    fn spec(layer: Layer) -> PlotSpec {
        PlotSpec::single(
            DataRef::Dataset {
                dataset: DatasetId::new(),
            },
            layer,
        )
    }

    fn scatter() -> Layer {
        Layer::new(Mark::Point, Stat::Identity)
            .with(Channel::X, FieldDef::of("area"))
            .with(Channel::Y, FieldDef::of("price"))
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let mut s = spec(scatter().with(Channel::Color, FieldDef::of("street")));
        s.scales.insert(
            Channel::Y,
            Scale {
                kind: ScaleKind::Log,
                ..Scale::default()
            },
        );
        s.facet.wrap = Some(FieldDef::binned("year_built"));
        s.references.push(Reference {
            channel: Channel::Y,
            value: json!(100000),
            label: None,
        });
        s.layers.push(
            Layer::new(
                Mark::Line,
                Stat::smooth(crate::plot::spec::SmoothMethod::Loess),
            )
            .with(Channel::X, FieldDef::of("area"))
            .with(Channel::Y, FieldDef::of("price")),
        );
        s.selections.push(Selection {
            name: "brush".into(),
            kind: SelectionKind::Interval,
            channels: vec![Channel::X],
        });
        let text = serde_json::to_value(&s).unwrap();
        assert_eq!(text["scales"]["y"]["kind"], json!("log"));
        assert_eq!(text["facet"]["wrap"]["bin"], json!({}));
        assert_eq!(text["layers"][1]["stat"]["kind"], json!("smooth"));
        // The identity stat and the defaults are left out of what is stored.
        assert!(text["layers"][0].get("stat").is_none());
        let back: PlotSpec = serde_json::from_value(text).unwrap();
        assert_eq!(back, s);
        assert!(
            validate(&s, &houses()).is_empty(),
            "{:?}",
            validate(&s, &houses())
        );

        // What a person writes by hand reads, with the defaults filled in.
        let typed: PlotSpec = serde_json::from_value(json!({
            "data": { "kind": "dataset", "dataset": DatasetId::new() },
            "layers": [{ "mark": "box", "stat": { "kind": "boxplot" },
                         "encoding": { "x": { "field": "street" }, "y": { "field": "price" } } }]
        }))
        .unwrap();
        assert_eq!(typed.layers[0].stat, Stat::boxplot());
        assert_eq!(typed.coord, Coord::Cartesian);
    }

    #[test]
    fn a_missing_column_is_named_with_the_columns_there_are() {
        let s =
            spec(Layer::new(Mark::Point, Stat::Identity).with(Channel::X, FieldDef::of("nope")));
        let problems = validate(&s, &houses());
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].starts_with(
                "X: `nope` is not a column of the dataset (its columns are `id`, `price`"
            ),
            "{problems:?}"
        );
    }

    #[test]
    fn types_must_suit_their_channels() {
        let cases: Vec<(Layer, &str)> = vec![
            (
                scatter().with(Channel::Size, FieldDef::of("street")),
                "Size shows an amount, and `street` is text",
            ),
            (
                scatter().with(Channel::Shape, FieldDef::of("area")),
                "Shape needs a column with a few values",
            ),
            (
                Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::binned("street")),
                "`street` is text, and only numbers can be binned",
            ),
            (
                Layer::new(Mark::Point, Stat::Identity).with(Channel::X, FieldDef::of("extra")),
                "`extra` is JSON, which cannot be plotted",
            ),
            (
                Layer::new(Mark::Box, Stat::boxplot())
                    .with(Channel::X, FieldDef::of("street"))
                    .with(Channel::Y, FieldDef::of("sold")),
                "a box plot needs numbers on Y, and `sold` is a boolean",
            ),
            (
                Layer::new(Mark::Bar, Stat::aggregate(AggregateFn::Mean))
                    .with(Channel::X, FieldDef::of("street"))
                    .with(Channel::Y, FieldDef::of("built_on")),
                "the mean needs numbers, and `built_on` is a date",
            ),
            (
                Layer::new(Mark::Box, Stat::boxplot())
                    .with(Channel::X, FieldDef::of("street"))
                    .with(Channel::Y, FieldDef::of("price"))
                    .with(Channel::Color, FieldDef::of("area")),
                "Color groups the rows here, and `area` is a number with many values; bin it",
            ),
            (
                Layer::new(Mark::Line, Stat::density())
                    .with(Channel::X, FieldDef::of("price"))
                    .with(Channel::Y, FieldDef::of("area")),
                "a density draws its own Y",
            ),
            (
                Layer::new(Mark::Line, Stat::Identity).with(Channel::X, FieldDef::of("area")),
                "A line needs a column on both X and Y",
            ),
            (
                Layer::new(Mark::Point, Stat::boxplot()).with(Channel::Y, FieldDef::of("price")),
                "a box plot cannot be drawn as points",
            ),
            (
                Layer::new(Mark::Box, Stat::Identity).with(Channel::Y, FieldDef::of("price")),
                "Box plots are drawn from a summary",
            ),
            (
                Layer::new(
                    Mark::Line,
                    Stat::smooth(crate::plot::spec::SmoothMethod::Linear),
                )
                .with(Channel::X, FieldDef::of("area")),
                "a smoother needs a column on Y",
            ),
        ];
        for (layer, expected) in cases {
            let problems = validate(&spec(layer.clone()), &houses());
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "{layer:?}\n expected {expected:?}\n got {problems:?}"
            );
        }
    }

    #[test]
    fn a_count_is_drawn_on_the_first_free_channel() {
        let shape = houses();
        let facet = Facet::default();
        let bars = Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::of("street"));
        assert_eq!(plan(&bars, &facet, &shape).unwrap().out, Some(Channel::Y));
        let across = Layer::new(Mark::Bar, Stat::Count).with(Channel::Y, FieldDef::binned("price"));
        assert_eq!(plan(&across, &facet, &shape).unwrap().out, Some(Channel::X));
        let heat = Layer::new(Mark::Rect, Stat::Count)
            .with(Channel::X, FieldDef::of("street"))
            .with(Channel::Y, FieldDef::of("sold"));
        let p = plan(&heat, &facet, &shape).unwrap();
        assert_eq!(p.out, Some(Channel::Color));
        assert_eq!(p.dims.len(), 2);
        let full = heat.clone().with(Channel::Color, FieldDef::of("street"));
        assert!(plan(&full, &facet, &shape).unwrap_err()[0].contains("nowhere to draw the count"));
    }

    #[test]
    fn facets_scales_selections_and_folds_are_checked() {
        let mut s = spec(scatter());
        s.facet.wrap = Some(FieldDef::of("price"));
        s.facet.row = Some(FieldDef::of("street"));
        s.scales.insert(Channel::Shape, Scale::default());
        s.scales.insert(
            Channel::X,
            Scale {
                kind: ScaleKind::Log,
                ..Scale::default()
            },
        );
        s.layers[0].encoding.x = Some(FieldDef::of("street"));
        s.selections.push(Selection {
            name: "pick".into(),
            kind: SelectionKind::Interval,
            channels: vec![Channel::Color],
        });
        let problems = validate(&s, &houses());
        for expected in [
            "Wrap cannot be combined with Facet rows",
            "Wrap makes one plot per value, and `price` is a number with many values; bin it",
            "Shape has no scale",
            "a log scale on X needs numbers, and `street` is text",
            "brushes a range, which is along X or Y, not Color",
            "filters on Color, which shows no column",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "expected {expected:?} in {problems:?}"
            );
        }

        // A fold makes `variable` and `value` out of number columns only.
        let mut folded = spec(
            Layer::new(Mark::Line, Stat::Identity)
                .with(Channel::X, FieldDef::of("year_built"))
                .with(Channel::Y, FieldDef::of("value"))
                .with(Channel::Color, FieldDef::of("variable")),
        );
        folded.fold = Some(Fold::of(["price", "area"]));
        assert!(validate(&folded, &houses()).is_empty());
        let shape = folded_shape(&houses(), folded.fold.as_ref()).unwrap();
        assert!(shape.column("price").is_none());
        assert_eq!(shape.column("value").unwrap().ty, ColType::Float);
        folded.fold = Some(Fold::of(["price", "street"]));
        assert_eq!(
            validate(&folded, &houses()),
            vec!["`street` is text, and only numbers can be compared as one variable".to_owned()]
        );
        folded.fold = Some(Fold {
            columns: vec!["price".into(), "area".into()],
            key: "street".into(),
            value: "value".into(),
            pairs: None,
        });
        assert!(
            validate(&folded, &houses())[0]
                .contains("makes a column `street`, and the dataset already has one")
        );
    }
}
