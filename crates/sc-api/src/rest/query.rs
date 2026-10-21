//! The REST read query string (design §13.4).
//!
//! `?select=title,author(name,country)&published=gte.2020-01-01&order=published.desc&limit=20`
//! parses here, and it parses into the row layer's own
//! [`RowQuery`](crate::rows::RowQuery) — the *same* value the GraphQL provider's
//! list arguments lower to, run through the *same*
//! [`ownership::read_row_values_as`]. REST is a **syntax over the read layer**,
//! not a second reader: one place decides what a filter may say (`crate::filter`,
//! shared with GraphQL), one place decides which rows a caller may see, and one
//! statement answers the request.
//!
//! **The subset is stated, and everything outside it is refused by name.** Taken
//! from PostgREST: `select` with embeds through outgoing keys to any depth,
//! `alias:column` renaming, `column=op.value` filters, `order=column.desc`,
//! `limit`, `offset`. Not taken: one-to-many embeds (a second, batched read),
//! `!inner` (it changes which parents come back — that is a join, and this read
//! is one table plus correlated subqueries), the `...` spread operator, `::`
//! casts, `or=(…)`, and filters on an embedded resource. A caller who asks for
//! one of those gets a `400` **naming it**, because the alternative — ignoring
//! what we do not understand — answers with rows the caller did not ask for, and
//! a silently dropped filter is the worst failure this API can have.
//!
//! An embed is not a second query either: `author(name,country)` becomes one
//! `sc-expr` Ⱶ-join correlated subquery per requested leaf, projected as extra
//! columns of the same `SELECT` and nested back into `{"author": {"name": …}}`
//! on the way out. Which is why every embed passes
//! [`ownership::join_guard`](crate::ownership::join_guard) first: a subquery has
//! no `WHERE` this provider owns, so a caller whose access to the target table
//! comes from an ownership formula is refused by name rather than handed a
//! withheld row one column at a time.

use std::collections::BTreeMap;

use sc_catalog::{Catalog, DataFieldKind, Table};
use sc_error::{Error, Result};
use sc_expr::JOIN;
use sc_query::{Projection, Value};
use serde_json::{Map, Value as Json};

use crate::convert::value_to_json;
use crate::ownership;
use crate::provider::ApiRequest;
use crate::query_string::{self, KEY_SELECT};
use crate::rows::RowQuery;

/// How deep a `select` may nest embeds.
///
/// Not a policy so much as a floor under the parser: `select` is recursive and
/// arrives from the wire, so a query string of a few kilobytes could otherwise
/// nest a thousand levels and take the stack with it. Nobody writes four.
const MAX_EMBED_DEPTH: usize = 8;

/// A parsed list request: which rows to read, and what shape to answer in.
pub(crate) struct ListQuery {
    /// The read itself — filter, order, bounds, and the embeds' projections.
    pub(crate) query: RowQuery,
    /// The projection the caller asked for, or `None` for the whole row.
    shape: Option<Vec<Node>>,
}

/// One node of a parsed `select`, resolved against the catalog.
#[derive(Debug, PartialEq)]
enum Node {
    /// A column: the key it answers under, and the key it arrives under in the
    /// read's value map (its own name at the root, a Ⱶ-join path below one).
    Leaf { key: String, path: String },
    /// An embedded related row, addressed through an outgoing key.
    Embed {
        /// The key it answers under.
        key: String,
        /// The **foreign key's own** path, which is what decides whether the
        /// related row is there at all: a null key answers `null` rather than an
        /// object of nulls, and rather than an error.
        path: String,
        /// What was asked for on the far side.
        children: Vec<Node>,
    },
}

impl ListQuery {
    /// The rows a read returned, rendered as the caller's response.
    ///
    /// Without a `select` this is the wire row every other REST read answers
    /// with. With one it is exactly what was asked for, in the caller's own keys
    /// and nesting — the join values are columns of the same statement, so the
    /// nesting happens here rather than in the database.
    ///
    /// A secret answers as its sentinel (TODO-imap M2): a client never reads one.
    pub(crate) fn render(&self, table: &Table, rows: &[BTreeMap<String, Value>]) -> Json {
        Json::Array(
            rows.iter()
                .map(|values| {
                    let mut values = values.clone();
                    sc_catalog::secrets::redact_values(table, &mut values);
                    match &self.shape {
                        None => ownership::table_row_json(table, &values),
                        Some(nodes) => render_row(nodes, &values),
                    }
                })
                .collect(),
        )
    }
}

/// One row through one `select` shape.
fn render_row(nodes: &[Node], values: &BTreeMap<String, Value>) -> Json {
    let mut map = Map::new();
    for node in nodes {
        match node {
            Node::Leaf { key, path } => {
                map.insert(
                    key.clone(),
                    values.get(path).map_or(Json::Null, value_to_json),
                );
            }
            Node::Embed {
                key,
                path,
                children,
            } => {
                let related = match values.get(path) {
                    Some(Value::Null) | None => Json::Null,
                    Some(_) => render_row(children, values),
                };
                map.insert(key.clone(), related);
            }
        }
    }
    Json::Object(map)
}

/// Parse a list request's query string against `table`.
///
/// `row_cap` is the application's ceiling: an absent `limit` becomes it and a
/// present one is **clamped** to it, exactly as the GraphQL provider's is. A
/// caller's number is a request, not a permission — a list endpoint with no
/// bound is how a table ends up streamed into a response by accident.
pub(crate) fn parse(
    cat: &Catalog,
    table: &Table,
    req: &ApiRequest,
    role: u8,
    row_cap: u64,
) -> Result<ListQuery> {
    // The filters, the ordering and the bounds are the shared vocabulary
    // (`crate::query_string`); `select` below is the only half that is this
    // provider's own.
    let mut query = query_string::row_query(table, &req.query, row_cap)?;

    let Some(select) = req.query_get(KEY_SELECT) else {
        return Ok(ListQuery { query, shape: None });
    };
    let mut plan = Plan::default();
    let nodes = resolve(cat, table, &parse_select(select)?, None, role, &mut plan)?;
    if !plan.idents.is_empty() {
        let shape = cat.schema_shape()?;
        let mut extra = Vec::with_capacity(plan.idents.len());
        for ident in &plan.idents {
            let expr = sc_expr::join_path_expr(&shape, &table.name, ident).map_err(Error::from)?;
            extra.push(Projection::expr_as(expr, ident.clone()));
        }
        query = query.projecting(extra);
    }
    Ok(ListQuery {
        query: query.requiring_caller_context(plan.in_caller_context),
        shape: Some(nodes),
    })
}

/// What resolving a `select` accumulates beyond the shape itself.
#[derive(Default)]
struct Plan {
    /// The Ⱶ-join leaves to project, deduplicated — one correlated subquery
    /// each, in the same statement as the row.
    idents: Vec<String>,
    /// Whether an embed reached an RLS-protected table, and so whether the whole
    /// statement has to run with the caller's GUCs set: the target's policies
    /// are what decides the subquery, and outside a caller-context transaction
    /// they see no caller and yield nothing — a silent null.
    in_caller_context: bool,
}

impl Plan {
    /// Record a value to project. A column of the row itself is already in the
    /// `SELECT`, so only paths *behind* a join are projections.
    fn project(&mut self, prefix: Option<&str>, ident: &str) {
        if prefix.is_some() && !self.idents.iter().any(|i| i == ident) {
            self.idents.push(ident.to_owned());
        }
    }
}

/// One item of a `select` list, as written.
#[derive(Debug, PartialEq)]
struct Item<'a> {
    /// The `alias:` in front of it, if any.
    alias: Option<&'a str>,
    /// The column or relation name.
    name: &'a str,
    /// `Some` when it was written with parentheses — an embed, even an empty
    /// one, which is a different thing from a column of the same name.
    children: Option<Vec<Item<'a>>>,
}

/// Parse a `select` string into its items.
fn parse_select(src: &str) -> Result<Vec<Item<'_>>> {
    let mut cursor = Cursor { src, at: 0 };
    let items = parse_items(&mut cursor, 0)?;
    match cursor.peek() {
        None => Ok(items),
        Some(c) => Err(Error::invalid(format!(
            "`select`: unexpected `{c}` at position {}",
            cursor.at
        ))),
    }
}

/// A byte cursor over the `select` string.
struct Cursor<'a> {
    src: &'a str,
    at: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.src[self.at..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek() {
            self.at += c.len_utf8();
        }
    }
}

/// `item ("," item)*`.
fn parse_items<'a>(cursor: &mut Cursor<'a>, depth: usize) -> Result<Vec<Item<'a>>> {
    let mut items = vec![parse_item(cursor, depth)?];
    while cursor.peek() == Some(',') {
        cursor.bump();
        items.push(parse_item(cursor, depth)?);
    }
    Ok(items)
}

/// `(alias ":")? name ("(" items ")")?`.
fn parse_item<'a>(cursor: &mut Cursor<'a>, depth: usize) -> Result<Item<'a>> {
    let first = parse_name(cursor)?;
    let (alias, name) = if cursor.peek() == Some(':') {
        cursor.bump();
        (Some(first), parse_name(cursor)?)
    } else {
        (None, first)
    };
    let children = if cursor.peek() == Some('(') {
        if depth + 1 > MAX_EMBED_DEPTH {
            return Err(Error::invalid(format!(
                "`select`: embeds nest more than {MAX_EMBED_DEPTH} levels deep"
            )));
        }
        cursor.bump();
        // `name()` is an embed of nothing rather than a parse error, so the
        // refusal below can say what it is about.
        let inner = if cursor.peek() == Some(')') {
            Vec::new()
        } else {
            parse_items(cursor, depth + 1)?
        };
        if cursor.peek() != Some(')') {
            return Err(Error::invalid(format!(
                "`select`: `{name}(` is never closed"
            )));
        }
        cursor.bump();
        Some(inner)
    } else {
        None
    };
    Ok(Item {
        alias,
        name,
        children,
    })
}

/// A bare name, up to the next `,`, `(`, `)` or `:` — with each not-taken piece
/// of PostgREST's grammar refused by the name it is known by.
fn parse_name<'a>(cursor: &mut Cursor<'a>) -> Result<&'a str> {
    let start = cursor.at;
    while let Some(c) = cursor.peek() {
        if matches!(c, ',' | '(' | ')' | ':') {
            break;
        }
        cursor.bump();
    }
    let name = cursor.src[start..cursor.at].trim();
    if name.starts_with("...") {
        return Err(Error::invalid(
            "`select`: the `...` spread operator is not taken by this API",
        ));
    }
    if let Some((column, modifier)) = name.split_once('!') {
        return Err(Error::invalid(format!(
            "`select`: `!{modifier}` on `{column}` is not taken by this API — a read here is \
             one table plus correlated subqueries, and `!inner` changes which rows of it come \
             back"
        )));
    }
    if name.is_empty() {
        return Err(Error::invalid(
            "`select`: a column name is missing (an item is `column`, `alias:column` or \
             `relation(columns…)`)",
        ));
    }
    Ok(name)
}

/// Resolve parsed items against the catalog: which column each names, which
/// values the read has to project, and whether the caller may follow each embed.
fn resolve(
    cat: &Catalog,
    table: &Table,
    items: &[Item<'_>],
    prefix: Option<&str>,
    role: u8,
    plan: &mut Plan,
) -> Result<Vec<Node>> {
    let mut nodes = Vec::with_capacity(items.len());
    for item in items {
        let name = item.name;
        if name.contains("::") {
            return Err(Error::invalid(format!(
                "`select`: the `::` cast in `{name}` is not taken by this API"
            )));
        }
        let key = item.alias.unwrap_or(name).to_owned();
        let Some(field) = crate::user_rows::visible_field(table, name) else {
            // An embed of a *table* name rather than of one of this table's keys
            // is PostgREST's one-to-many embed: a second, batched read, and the
            // shape of paging and ordering *within* it is undecided. Named as
            // what it is, because "no such field" would send the caller looking
            // for a typo.
            if item.children.is_some() && cat.require(name).is_ok() {
                return Err(Error::invalid(format!(
                    "`select`: `{name}(…)` is a one-to-many embed, which this API does not \
                     take yet — read `{name}` with a filter of its own instead"
                )));
            }
            return Err(Error::invalid(format!(
                "`{}` has no field `{name}` to select",
                table.name
            )));
        };
        // A secret answers as its sentinel at the root; through an embed it
        // is not offered at all, since a joined value is a path and not a
        // field the redaction can recognise.
        if prefix.is_some() && field.is_secret() {
            return Err(Error::invalid(format!(
                "`select`: `{}`.`{name}` is a secret, which an embed does not select",
                table.name
            )));
        }
        let ident = match prefix {
            Some(prefix) => format!("{prefix}{JOIN}{name}"),
            None => name.to_owned(),
        };
        let Some(children) = &item.children else {
            plan.project(prefix, &ident);
            nodes.push(Node::Leaf { key, path: ident });
            continue;
        };
        let DataFieldKind::Key { target_table, .. } = &field.kind else {
            return Err(Error::invalid(format!(
                "`select`: `{}`.`{name}` is not a key to another table, so `{name}(…)` \
                 embeds nothing",
                table.name
            )));
        };
        let target = cat.require(&target_table.0)?;
        // A joined row is *read*, so the target's read rule holds — an embed
        // must not become a way around the floor a list over the same table
        // enforces, and a formula-granted access cannot be carried by a
        // subquery at all (see `join_guard`).
        match ownership::join_guard(&target, role)? {
            ownership::JoinAccess::Unrestricted => {}
            ownership::JoinAccess::InContext => plan.in_caller_context = true,
        }
        // The foreign key's own value, so a null relation answers `null` without
        // looking at a single leaf. At the root it is already in the `SELECT`.
        plan.project(prefix, &ident);
        let children = resolve(cat, &target, children, Some(&ident), role, plan)?;
        nodes.push(Node::Embed {
            key,
            path: ident,
            children,
        });
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn select_parses_leaves_embeds_aliases_and_nesting() {
        let items = parse_select("title,name:author(country,publisher(city))").expect("parses");
        assert_eq!(
            items,
            vec![
                Item {
                    alias: None,
                    name: "title",
                    children: None
                },
                Item {
                    alias: Some("name"),
                    name: "author",
                    children: Some(vec![
                        Item {
                            alias: None,
                            name: "country",
                            children: None
                        },
                        Item {
                            alias: None,
                            name: "publisher",
                            children: Some(vec![Item {
                                alias: None,
                                name: "city",
                                children: None
                            }])
                        },
                    ])
                },
            ]
        );
    }

    #[test]
    fn the_not_taken_select_grammar_is_refused_by_the_name_it_is_known_by() {
        let inner = parse_select("author!inner(name)").unwrap_err().to_string();
        assert!(inner.contains("`!inner`"), "{inner}");
        let spread = parse_select("...author(name)").unwrap_err().to_string();
        assert!(spread.contains("spread operator"), "{spread}");
        let unclosed = parse_select("author(name").unwrap_err().to_string();
        assert!(unclosed.contains("is never closed"), "{unclosed}");
        let empty = parse_select("title,").unwrap_err().to_string();
        assert!(empty.contains("a column name is missing"), "{empty}");
        // Deep nesting is refused rather than taking the stack with it.
        let deep = "a(".repeat(20) + &")".repeat(20);
        assert!(parse_select(&deep).is_err());
    }

    #[test]
    fn a_null_key_renders_as_a_null_object_rather_than_an_object_of_nulls() {
        let nodes = vec![
            Node::Leaf {
                key: "title".into(),
                path: "title".into(),
            },
            Node::Embed {
                key: "author".into(),
                path: "author".into(),
                children: vec![Node::Leaf {
                    key: "name".into(),
                    path: format!("author{JOIN}name"),
                }],
            },
        ];
        let mut values = BTreeMap::new();
        values.insert("title".to_owned(), Value::Text("Emma".into()));
        values.insert("author".to_owned(), Value::Null);
        assert_eq!(
            render_row(&nodes, &values),
            serde_json::json!({ "title": "Emma", "author": Json::Null })
        );

        values.insert("author".to_owned(), Value::Int(3));
        values.insert(format!("author{JOIN}name"), Value::Text("Austen".into()));
        assert_eq!(
            render_row(&nodes, &values),
            serde_json::json!({ "title": "Emma", "author": { "name": "Austen" } })
        );
    }
}
