//! `feldspar api` — an application's **custom queries** (SQL, or a JavaScript
//! or Python body) from the command line (§13.4).
//!
//! The admin UI can add one; so can this, and for the reasons `build-app`
//! exists: it is scriptable, it is what a deploy step or a coding agent calls,
//! and it works when there is no browser pointed at the server. The generated
//! `src/feldspar/README.md` tells an agent working in an app's project to use it,
//! which is the same reason there are three commands rather than one — an
//! add-only command is a trap, because the first typo would need a browser to
//! fix, which is exactly the situation the command exists to avoid.
//!
//! This module is the **parsing**: turning the flags into a [`CustomQuery`].
//! Which API row holds it is [`sc_app::select_api`]'s, shared with the
//! `admin_copilot` trait that writes one from a conversation (§11.3) — one rule,
//! so a query cannot land in one place from the command line and another from an
//! agent. Connecting, saving and re-emitting the client are the binary's, so this
//! half is testable without a database.

use sc_api::{CustomParam, CustomQuery, Method, QueryLanguage, ValueType};
use sc_error::{Error, Result};

/// `feldspar api add-query`'s arguments, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct AddQueryArgs {
    /// `--app`: which application, by subdomain.
    pub app: String,
    /// `--api`: which of its APIs, by mount. Optional when only one of them
    /// serves custom queries.
    pub api: Option<String>,
    /// The query itself, as the flags describe it.
    pub query: CustomQuery,
}

/// `feldspar api list-queries` / `remove-query`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryRefArgs {
    /// `--app`: which application, by subdomain.
    pub app: String,
    /// `--api`: which of its APIs, by mount.
    pub api: Option<String>,
    /// `--name`: which query. Required by `remove-query`, unused by
    /// `list-queries`.
    pub name: Option<String>,
}

/// Parse `add-query`'s flags.
///
/// ```text
/// --app SUBDOMAIN [--api MOUNT] --name NAME [--method GET] --path /sub/path
/// [--min-role N] [--description TEXT] [--param name:type[,name:type…]]…
/// [--language sql|javascript|python] (--sql|--code) TEXT|@FILE
/// ```
///
/// `--code` and `--sql` are the same flag: the source, in whichever language
/// `--language` names (SQL unless it says otherwise). Both spellings, because
/// `--sql` holding a JavaScript body reads as a mistake, and a script that
/// already says `--sql` should not have to change.
///
/// The defaults are the model's own: `GET`, and the **admin** role floor. An
/// unstated floor is admin because that is what an unstated one means everywhere
/// else a query is written (§10.2) — a command-line default of "public" would be
/// the one place the rule did not hold.
pub fn parse_add_query(args: &[String]) -> Result<AddQueryArgs> {
    let mut app = None;
    let mut api = None;
    let mut name = None;
    let mut method = None;
    let mut path = None;
    let mut min_role = None;
    let mut description = None;
    let mut language = None;
    let mut source = None;
    let mut params: Vec<CustomParam> = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--app" => app = Some(value(&mut it, arg)?),
            "--api" => api = Some(value(&mut it, arg)?),
            "--name" => name = Some(value(&mut it, arg)?),
            "--method" => method = Some(value(&mut it, arg)?),
            "--path" => path = Some(value(&mut it, arg)?),
            "--min-role" => min_role = Some(value(&mut it, arg)?),
            "--description" => description = Some(value(&mut it, arg)?),
            "--language" => language = Some(value(&mut it, arg)?),
            "--sql" | "--code" => {
                if source.is_some() {
                    return Err(Error::config(
                        "give the query's source once, with --code or --sql",
                    ));
                }
                source = Some(value(&mut it, arg)?);
            }
            "--param" => {
                for spec in value(&mut it, arg)?.split(',') {
                    params.push(parse_param(spec.trim())?);
                }
            }
            other => {
                return Err(Error::config(format!(
                    "unknown add-query argument `{other}`"
                )));
            }
        }
    }

    let app = required(app, "--app")?;
    let name = required(name, "--name")?;
    let path = required(path, "--path")?;
    let source = read_source(&source.ok_or_else(|| {
        Error::config("--code (or --sql) is required: the query's source, or @FILE")
    })?)?;
    let language = match language {
        Some(l) => parse_language(&l)?,
        None => QueryLanguage::Sql,
    };
    let method = match method {
        Some(m) => parse_method(&m)?,
        None => Method::Get,
    };
    let min_role = match min_role {
        Some(r) => r.parse::<u8>().map_err(|_| {
            Error::config(format!("--min-role wants a number in 1..=100, not `{r}`"))
        })?,
        None => sc_auth::ROLE_ADMIN,
    };

    let mut query = CustomQuery::new(name, method, path, source)
        .language(language)
        .params(params)
        .min_role(min_role);
    if let Some(description) = description {
        query = query.description(description);
    }
    Ok(AddQueryArgs { app, api, query })
}

/// Parse the `--app`/`--api`/`--name` flags `list-queries` and `remove-query`
/// share.
pub fn parse_query_ref(command: &str, args: &[String]) -> Result<QueryRefArgs> {
    let mut app = None;
    let mut api = None;
    let mut name = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--app" => app = Some(value(&mut it, arg)?),
            "--api" => api = Some(value(&mut it, arg)?),
            "--name" => name = Some(value(&mut it, arg)?),
            other => {
                return Err(Error::config(format!(
                    "unknown {command} argument `{other}`"
                )));
            }
        }
    }
    Ok(QueryRefArgs {
        app: required(app, "--app")?,
        api,
        name,
    })
}

/// One `--param name:type` or `name:type?` (the `?` marking it optional).
///
/// An optional parameter binds SQL `NULL` when the caller leaves it out, which
/// is what makes `(:q is null or name = :q)` the idiom for an optional filter —
/// so the spelling that says "may be omitted" is worth having on the command
/// line too.
fn parse_param(spec: &str) -> Result<CustomParam> {
    let (name, ty) = spec.split_once(':').ok_or_else(|| {
        Error::config(format!(
            "--param wants `name:type` (e.g. `since:int`, or `q:text?` for an \
             optional one), not `{spec}`"
        ))
    })?;
    let (ty, required) = match ty.strip_suffix('?') {
        Some(ty) => (ty, false),
        None => (ty, true),
    };
    let ty = ValueType::from_name(ty.trim()).ok_or_else(|| {
        Error::config(format!(
            "`{ty}` is not a parameter type; use one of {}",
            ValueType::ALL
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let param = CustomParam::new(name.trim(), ty);
    Ok(if required { param } else { param.optional() })
}

/// `--code`/`--sql`'s value: the source itself, or `@path` naming a file
/// holding it.
///
/// A file, because a query is many lines and a shell is a poor place to keep
/// them — the query an admin actually wants is usually already in a `.sql` or
/// `.js` file beside the project, and `--code @report.js` is how it gets in
/// without a heredoc.
fn read_source(value: &str) -> Result<String> {
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_owned());
    };
    std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("cannot read the query file `{path}`: {e}")))
}

/// `--language`'s value, as the model's enum.
fn parse_language(raw: &str) -> Result<QueryLanguage> {
    match raw.to_ascii_lowercase().as_str() {
        "sql" => Ok(QueryLanguage::Sql),
        "javascript" | "js" => Ok(QueryLanguage::Javascript),
        "python" | "py" => Ok(QueryLanguage::Python),
        other => Err(Error::config(format!(
            "`{other}` is not a query language; use sql, javascript or python"
        ))),
    }
}

/// `--method`'s value, as the model's enum.
fn parse_method(raw: &str) -> Result<Method> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "PATCH" => Ok(Method::Patch),
        "DELETE" => Ok(Method::Delete),
        other => Err(Error::config(format!(
            "`{other}` is not an HTTP method; use GET, POST, PUT, PATCH or DELETE"
        ))),
    }
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String> {
    it.next()
        .cloned()
        .ok_or_else(|| Error::config(format!("{flag} requires a value")))
}

fn required(value: Option<String>, flag: &str) -> Result<String> {
    value.ok_or_else(|| Error::config(format!("{flag} is required")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn base() -> Vec<String> {
        args(&[
            "--app",
            "blog",
            "--name",
            "topAuthors",
            "--path",
            "/reports/top-authors",
            "--sql",
            "select author from books where year > :since",
            "--param",
            "since:int",
        ])
    }

    #[test]
    fn it_parses_a_query_and_defaults_to_get_at_the_admin_floor() {
        let parsed = parse_add_query(&base()).unwrap();
        assert_eq!(parsed.app, "blog");
        assert_eq!(parsed.api, None);
        assert_eq!(parsed.query.name, "topAuthors");
        assert_eq!(parsed.query.method, Method::Get);
        assert_eq!(parsed.query.path, "/reports/top-authors");
        assert_eq!(parsed.query.min_role, sc_auth::ROLE_ADMIN);
        assert_eq!(
            parsed.query.params,
            vec![CustomParam::new("since", ValueType::Int)]
        );
    }

    #[test]
    fn several_parameters_may_share_one_flag_or_take_one_each() {
        let mut one = base();
        one.extend(args(&["--param", "q:text?,limit:int"]));
        let parsed = parse_add_query(&one).unwrap();
        assert_eq!(
            parsed.query.params,
            vec![
                CustomParam::new("since", ValueType::Int),
                // The `?` is what says "may be omitted", and an omitted one binds
                // NULL rather than being refused.
                CustomParam::new("q", ValueType::Text).optional(),
                CustomParam::new("limit", ValueType::Int),
            ]
        );
    }

    #[test]
    fn a_parameter_type_that_is_not_one_is_refused_naming_the_choices() {
        let mut bad = base();
        bad.extend(args(&["--param", "n:integer"]));
        let msg = parse_add_query(&bad).unwrap_err().to_string();
        assert!(msg.contains("integer"), "{msg}");
        assert!(msg.contains("int,"), "{msg}");

        let mut worse = base();
        worse.extend(args(&["--param", "n"]));
        assert!(parse_add_query(&worse).is_err());
    }

    #[test]
    fn the_sql_may_come_from_a_file() {
        let dir = std::env::temp_dir().join(format!("sc-cli-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.sql");
        std::fs::write(&path, "select 1 as n").unwrap();

        let mut with_file = args(&["--app", "blog", "--name", "r", "--path", "/r"]);
        with_file.extend(args(&["--sql", &format!("@{}", path.display())]));
        assert_eq!(
            parse_add_query(&with_file).unwrap().query.code,
            "select 1 as n"
        );

        // …and a file that is not there is a *config* error naming it, not a
        // query saved with the literal text `@/tmp/nope.sql`.
        let mut missing = args(&["--app", "blog", "--name", "r", "--path", "/r"]);
        missing.extend(args(&["--sql", "@/nonexistent/nope.sql"]));
        let msg = parse_add_query(&missing).unwrap_err().to_string();
        assert!(msg.contains("nope.sql"), "{msg}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_code_query_takes_its_language_and_its_body_from_a_file() {
        let dir = std::env::temp_dir().join(format!("sc-cli-api-code-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.js");
        std::fs::write(&path, "return { n: 1 };").unwrap();

        let mut js = args(&["--app", "blog", "--name", "stats", "--path", "/stats"]);
        js.extend(args(&["--language", "javascript"]));
        js.extend(args(&["--code", &format!("@{}", path.display())]));
        let query = parse_add_query(&js).unwrap().query;
        assert_eq!(query.language, QueryLanguage::Javascript);
        assert_eq!(query.code, "return { n: 1 };");
        assert!(query.is_code());

        // `--code` with no `--language` is SQL, as `--sql` always was.
        let plain = args(&[
            "--app", "blog", "--name", "r", "--path", "/r", "--code", "select 1",
        ]);
        assert_eq!(
            parse_add_query(&plain).unwrap().query.language,
            QueryLanguage::Sql
        );

        // The short spellings, and a language there is no such thing as.
        let mut py = args(&["--app", "blog", "--name", "p", "--path", "/p"]);
        py.extend(args(&["--language", "py", "--code", "return 1"]));
        assert_eq!(
            parse_add_query(&py).unwrap().query.language,
            QueryLanguage::Python
        );
        let mut bad = args(&["--app", "blog", "--name", "p", "--path", "/p"]);
        bad.extend(args(&["--language", "cobol", "--code", "x"]));
        let msg = parse_add_query(&bad).unwrap_err().to_string();
        assert!(msg.contains("cobol"), "{msg}");

        // The source is given once: `--sql` and `--code` together is refused,
        // not one of them silently dropped.
        let mut both = args(&["--app", "blog", "--name", "p", "--path", "/p"]);
        both.extend(args(&["--sql", "select 1", "--code", "return 1"]));
        assert!(parse_add_query(&both).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_required_flag_and_an_unknown_one_are_both_refused() {
        for missing in ["--app", "--name", "--path", "--sql"] {
            let kept: Vec<String> = {
                let all = base();
                let mut out = Vec::new();
                let mut it = all.into_iter();
                while let Some(a) = it.next() {
                    if a == missing {
                        it.next();
                        continue;
                    }
                    out.push(a);
                }
                out
            };
            let msg = parse_add_query(&kept).unwrap_err().to_string();
            assert!(msg.contains(missing), "{missing}: {msg}");
        }
        let mut unknown = base();
        unknown.extend(args(&["--role", "40"]));
        let msg = parse_add_query(&unknown).unwrap_err().to_string();
        assert!(msg.contains("--role"), "{msg}");

        // A flag whose value is missing is not a flag with an empty value.
        assert!(parse_add_query(&args(&["--app"])).is_err());
    }

    #[test]
    fn the_method_and_the_role_floor_are_the_admins_choice() {
        let mut chosen = base();
        chosen.extend(args(&["--method", "post", "--min-role", "40"]));
        let parsed = parse_add_query(&chosen).unwrap();
        assert_eq!(parsed.query.method, Method::Post);
        assert_eq!(parsed.query.min_role, 40);

        let mut bad = base();
        bad.extend(args(&["--method", "FETCH"]));
        assert!(parse_add_query(&bad).is_err());
        let mut worse = base();
        worse.extend(args(&["--min-role", "everyone"]));
        assert!(parse_add_query(&worse).is_err());
    }

    #[test]
    fn list_queries_takes_the_app_and_an_optional_api() {
        let parsed = parse_query_ref("list-queries", &args(&["--app", "blog"])).unwrap();
        assert_eq!(parsed.app, "blog");
        assert_eq!(parsed.api, None);
        assert!(parse_query_ref("list-queries", &args(&["--api", "/api"])).is_err());
    }
}
