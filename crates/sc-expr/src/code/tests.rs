use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use std::sync::atomic::AtomicU32;

/// How a [`FakeHost`] answers one plan.
type Answer = Box<dyn Fn(&Json) -> Result<Json> + Send + Sync>;

/// A host that records every plan it was asked for and answers from a
/// closure. The point of the seam: this crate can be tested end to end
/// without a catalog, a database or `sc-api`.
struct FakeHost {
    plans: Mutex<Vec<Json>>,
    answer: Answer,
    delay: Option<Duration>,
    calls: AtomicU32,
}

impl FakeHost {
    fn new(answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static) -> Arc<FakeHost> {
        Arc::new(FakeHost {
            plans: Mutex::new(Vec::new()),
            answer: Box::new(answer),
            delay: None,
            calls: AtomicU32::new(0),
        })
    }

    fn rows(rows: Json) -> Arc<FakeHost> {
        FakeHost::new(move |_| Ok(rows.clone()))
    }

    fn plans(&self) -> Vec<Json> {
        self.plans.lock().unwrap().clone()
    }
}

#[async_trait]
impl CodeHost for FakeHost {
    async fn call(&self, request: Json) -> Result<Json> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.plans.lock().unwrap().push(request.clone());
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        (self.answer)(&request)
    }
}

fn call(code: &str) -> CodeCall<'static> {
    CodeCall {
        code: code.to_owned(),
        ..CodeCall::default()
    }
}

/// A call against a host the caller keeps: the host is borrowed (§the
/// bridge), so the `Arc` these tests hold is what owns it.
fn with_host<'a>(code: &str, host: &'a dyn CodeHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        host: Some(host),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_with_no_host_is_the_pure_body_it_always_was() {
    let rt = CodeRuntime::new();
    // Statements, a local declaration and a `return` — the thing a formula
    // (one expression) cannot be.
    let mut c = call("let t = 0; for (const n of payload.ns) t += n; return t;");
    c.bindings.insert("payload".into(), json!({ "ns": [2, 5] }));
    assert_eq!(rt.run(c).await.unwrap(), json!(7));
    // And `db` is not merely inert, it is absent: naming it is a
    // ReferenceError naming it, not a handle that fails on use.
    let err = rt
        .run(call("return await db.books.rows();"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("JavaScript code failed") && err.contains("db"),
        "{err}"
    );
}

/// A host that answers a model handle's requests the way `sc-api`'s does:
/// `get` with the model `model_of` describes, `predict` with each row's
/// `id` (or `"new"`) times ten, and anything else with the request itself.
fn model_host(model_of: fn(&str) -> Json) -> Arc<FakeHost> {
    FakeHost::new(move |plan| {
        assert_eq!(plan["op"], json!("models"), "{plan}");
        let what = plan["what"].as_str().unwrap_or_default();
        Ok(match what {
            "get" => model_of(plan["model"].as_str().unwrap_or_default()),
            "predict" => Json::Array(
                plan["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| match row["id"].as_i64() {
                        Some(id) => json!(id * 10),
                        None => json!("new"),
                    })
                    .collect(),
            ),
            _ => plan.clone(),
        })
    })
}

/// `House prices`, a regression, and `Radon`, a posterior.
fn two_models(name: &str) -> Json {
    let fit = |id: &str| {
        json!({ "id": id, "name": "fit", "status": "fitted", "active": true,
                    "warnings": [], "metrics": {}, "parameters": [] })
    };
    match name {
        "Radon" => json!({
            "name": "Radon", "provider": "stan", "table": "homes",
            "outcome": { "outcome": "posterior" },
            "fit": fit("f-radon"),
            "variables": ["alpha", "beta"],
            "no_prediction": "`Radon` is a posterior: read its draws in a code body \
                              (`m.draws(…)`, on `models.get(…)`)",
        }),
        _ => json!({
            "name": name, "provider": "linear_regression", "table": "houses",
            "outcome": { "outcome": "regression", "label": "price" },
            "fit": fit("f-houses"),
            "variables": null,
            "no_prediction": null,
        }),
    }
}

/// Milestone 31 §3: `models.get` builds a handle from one request, and
/// every later call names the fit it resolved.
#[tokio::test]
async fn a_model_handle_predicts_a_row_or_an_array_naming_the_fit_it_was_built_on() {
    let rt = CodeRuntime::new();
    let host = model_host(two_models);
    let out = rt
        .run(with_host(
            r#"const m = await models.get("House prices");
                   return {
                     name: m.name, provider: m.provider, table: m.table,
                     outcome: m.outcome, fit: m.fit.id,
                     one: await m.predict({ id: 3 }),
                     many: await m.predict([{ id: 1 }, { area: 90 }, { id: 2 }]),
                     detail: await m.predict({ id: 4 }, { detail: true }),
                     keys: Object.keys(m).sort(),
                   };"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out["name"], json!("House prices"));
    assert_eq!(out["provider"], json!("linear_regression"));
    assert_eq!(out["table"], json!("houses"));
    assert_eq!(out["outcome"]["label"], json!("price"));
    assert_eq!(out["fit"], json!("f-houses"));
    assert_eq!(out["one"], json!(30));
    assert_eq!(out["many"], json!([10, "new", 20]));
    // The posterior methods are not among a regression's own keys.
    assert_eq!(
        out["keys"],
        json!([
            "asAdmin", "asUser", "fit", "name", "outcome", "predict", "provider", "table"
        ])
    );
    let plans = host.plans();
    assert_eq!(plans.len(), 4, "{plans:?}");
    assert_eq!(plans[0]["what"], json!("get"));
    assert_eq!(plans[0]["model"], json!("House prices"));
    assert!(plans[0].get("fit").is_none(), "{}", plans[0]);
    for plan in &plans[1..] {
        assert_eq!(plan["what"], json!("predict"));
        assert_eq!(plan["fit"], json!("f-houses"));
    }
    // One row is sent as a batch of one; an array as one request.
    assert_eq!(plans[1]["rows"], json!([{ "id": 3 }]));
    assert_eq!(plans[2]["rows"].as_array().unwrap().len(), 3);
    assert_eq!(plans[1]["detail"], json!(false));
    assert_eq!(plans[3]["detail"], json!(true));

    // A specific fit is asked for by id.
    let host = model_host(two_models);
    rt.run(with_host(
        r#"await models.get("House prices", { fit: "f-old" });"#,
        &*host,
    ))
    .await
    .unwrap();
    assert_eq!(host.plans()[0]["fit"], json!("f-old"));
}

#[tokio::test]
async fn a_posterior_only_method_on_another_model_throws_a_sentence_saying_what_it_is() {
    let rt = CodeRuntime::new();
    let host = model_host(two_models);
    let out = rt
        .run(with_host(
            r#"const m = await models.get("House prices");
                   const said = {};
                   for (const call of [
                     () => m.draws("alpha"),
                     () => m.summary("alpha"),
                     () => m.variables,
                     () => m.writePosterior({ variable: "alpha", statistics: {} }),
                   ]) {
                     try { call(); said.none = "no error"; }
                     catch (e) { said[Object.keys(said).length] = e.message; }
                   }
                   said.stringified = JSON.stringify(m).length > 0;
                   return said;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(
        out["0"],
        json!("`House prices` is a linear_regression regression; `draws` is for posterior models")
    );
    assert_eq!(
        out["2"],
        json!(
            "`House prices` is a linear_regression regression; `variables` is for \
                 posterior models"
        )
    );
    assert_eq!(
        out["3"],
        json!(
            "`House prices` is a linear_regression regression; `writePosterior` is for \
                 posterior models"
        )
    );
    assert!(out.get("none").is_none(), "{out}");
    // The throwing getters are not enumerable, so the handle serialises.
    assert_eq!(out["stringified"], json!(true));
    // And none of it reached the host.
    assert_eq!(host.plans().len(), 1);
}

#[tokio::test]
async fn a_posterior_handle_reads_draws_and_writes_back_under_the_handles_authority() {
    let rt = CodeRuntime::new();
    let host = model_host(two_models);
    let out = rt
        .run(with_host(
            r#"const m = await models.get("Radon");
                   const d = await m.draws("alpha", { keys: [27001], chains: [1, 2], thin: 10 });
                   const s = await m.summary("alpha", { elements: { "1": ["Aitkin"] } });
                   const w = await m.writePosterior({
                     variable: "alpha", statistics: { mean: "alpha_mean", sd: "alpha_sd" } });
                   const u = await m.asUser().writePosterior({
                     variable: "alpha", statistics: { mean: "alpha_mean" } });
                   let predicted;
                   try { await m.predict({ id: 1 }); } catch (e) { predicted = e.message; }
                   let both;
                   try { await m.draws("alpha", { keys: [1], elements: {} }); }
                   catch (e) { both = e.message; }
                   return { variables: m.variables, d, s, w, u, predicted, both };"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out["variables"], json!(["alpha", "beta"]));
    assert_eq!(out["d"]["what"], json!("draws"));
    assert_eq!(out["d"]["fit"], json!("f-radon"));
    assert_eq!(out["d"]["variable"], json!("alpha"));
    assert_eq!(out["d"]["elements"], json!({ "1": [27001] }));
    assert_eq!(out["d"]["chains"], json!([1, 2]));
    assert_eq!(out["d"]["thin"], json!(10));
    assert_eq!(out["s"]["what"], json!("summary"));
    assert_eq!(out["s"]["elements"], json!({ "1": ["Aitkin"] }));
    assert_eq!(out["w"]["what"], json!("write_posterior"));
    assert_eq!(out["w"]["authority"], json!("default"));
    assert_eq!(out["w"]["write"]["statistics"]["sd"], json!("alpha_sd"));
    assert_eq!(out["u"]["authority"], json!("user"));
    // A posterior does not predict rows, and says where to go instead,
    // without asking the host.
    assert!(
        out["predicted"].as_str().unwrap().contains("`m.draws(…)`"),
        "{out}"
    );
    assert_eq!(
        out["both"],
        json!("give m.draws() either `keys` or `elements`, not both")
    );
    assert!(host.plans().iter().all(|p| p["what"] != json!("predict")));
}

#[tokio::test]
async fn models_get_refuses_what_is_not_a_name_and_an_option_it_does_not_have() {
    let rt = CodeRuntime::new();
    let host = model_host(two_models);
    let out = rt
            .run(with_host(
                r#"const said = [];
                   for (const call of [
                     () => models.get(),
                     () => models.get("House prices", { instance: "x" }),
                     () => models.draws,
                   ]) {
                     try { const r = await call(); said.push(r === undefined ? "undefined" : "ok"); }
                     catch (e) { said.push(e.message); }
                   }
                   return said;"#,
                &*host,
            ))
            .await
            .unwrap();
    assert_eq!(
        out[0],
        json!(r#"models.get() takes a model's name, as in models.get("House prices")"#)
    );
    assert_eq!(
        out[1],
        json!("`instance` is not an option of models.get(); the options are: fit")
    );
    // The flat functions of the Stan milestone are gone.
    assert_eq!(out[2], json!("undefined"));
    assert!(host.plans().is_empty());
}

/// §1a: the node globals are shadowed as parameters of the wrapper, so a
/// body naming one sees `undefined` rather than whatever the isolate it
/// happens to run on has.
#[tokio::test]
async fn the_node_globals_are_shadowed_in_a_code_body() {
    let rt = CodeRuntime::new();
    // Every one of them, in one body, as the `typeof` the author would see.
    let entries = SHADOWED_NODE_GLOBALS
        .map(|name| format!("{name}: typeof {name}"))
        .join(", ");
    let out = rt
        .run(call(&format!("return {{ {entries} }};")))
        .await
        .unwrap();
    for name in SHADOWED_NODE_GLOBALS {
        assert_eq!(out[name], json!("undefined"), "{name} is not shadowed");
    }
    // Reading through one is the mistake it should be, on the line that made
    // it, rather than a value out of the runtime.
    let err = rt
        .run(call("return process.env.DATABASE_URL;"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("JavaScript code failed"), "{err}");

    // The surfaces a body *is* given are untouched…
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    assert_eq!(
        rt.run(with_host("return await db.books.rows();", &*host))
            .await
            .unwrap(),
        json!([{ "id": 1 }])
    );
    // …and a binding of a shadowed name still wins, because it is the
    // author's own and the parameter is only a default of `undefined`.
    let mut c = call("return module.answer;");
    c.bindings.insert("module".into(), json!({ "answer": 42 }));
    assert_eq!(rt.run(c).await.unwrap(), json!(42));
}

/// `console.log` is in scope for every body, and what it prints is kept for
/// whoever asked for it — the admin's Test run.
#[tokio::test]
async fn the_console_is_bound_and_what_it_prints_is_collected() {
    let rt = CodeRuntime::new();
    let sink = console_sink();
    let mut c = call(
        "console.log('hello', 42, { a: [1, 2] });\n\
             console.error(new Error('bad'));\n\
             console.warn('and', undefined);\n\
             return 'done';",
    );
    c.console = Some(Arc::clone(&sink));
    assert_eq!(rt.run(c).await.unwrap(), json!("done"));
    let lines = take_console(&sink);
    let said: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| (l.level.as_str(), l.text.as_str()))
        .collect();
    assert_eq!(said[0], ("log", "hello 42 {\"a\":[1,2]}"));
    assert_eq!(said[1].0, "error");
    assert!(said[1].1.contains("Error: bad"), "{:?}", said[1]);
    assert_eq!(said[2], ("warn", "and undefined"));
    // Taking the lines empties the sink: a second test run starts blank.
    assert!(take_console(&sink).is_empty());
}

/// The lines a **failing** body printed are the ones worth having, so they
/// are in the sink whether it answered or threw — and the excess is dropped
/// with a line that says so.
#[tokio::test]
async fn a_failed_body_leaves_its_console_behind_and_the_transcript_is_bounded() {
    let rt = CodeRuntime::new();
    let sink = console_sink();
    let mut c = call("console.log('before'); throw new Error('boom');");
    c.console = Some(Arc::clone(&sink));
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("boom"), "{err}");
    let lines = take_console(&sink);
    assert_eq!(lines[0].text, "before");

    let mut c = call("for (let i = 0; i < 500; i++) console.log(i); return null;");
    c.console = Some(Arc::clone(&sink));
    rt.run(c).await.unwrap();
    let lines = take_console(&sink);
    assert_eq!(lines.len(), MAX_CONSOLE_LINES + 1);
    assert!(
        lines[MAX_CONSOLE_LINES].text.contains("dropped"),
        "{lines:?}"
    );
}

/// No sink is not no console: an ordinary firing still runs, and a body that
/// binds its own `console` keeps it.
#[tokio::test]
async fn a_body_with_no_sink_still_has_a_console_and_a_binding_still_wins() {
    let rt = CodeRuntime::new();
    assert_eq!(
        rt.run(call(
            "console.log('to the log'); return typeof console.info;"
        ))
        .await
        .unwrap(),
        json!("function")
    );
    let mut c = call("return console.answer;");
    c.bindings.insert("console".into(), json!({ "answer": 42 }));
    assert_eq!(rt.run(c).await.unwrap(), json!(42));
}

#[tokio::test]
async fn a_chain_lowers_to_one_plan_and_one_round_trip() {
    let host = FakeHost::rows(json!([{ "id": 1, "amount": 3 }]));
    let rt = CodeRuntime::new();
    let out = rt
            .run(with_host(
                r#"return await db.invoices
                     .where({ paid: false, due: { lt: "2026-08-17" } })
                     .select("id", "amount", "customerⱵemail", { chased: "remindersↃinvoice.length" })
                     .orderBy("due")
                     .limit(50)
                     .offset(0)
                     .rows();"#,
                &*host,
            ))
            .await
            .unwrap();
    assert_eq!(out, json!([{ "id": 1, "amount": 3 }]));
    let plans = host.plans();
    assert_eq!(plans.len(), 1, "one terminal is one round trip");
    assert_eq!(
        plans[0],
        json!({
            "op": "select",
            "table": "invoices",
            "authority": "default",
            "where": { "paid": false, "due": { "lt": "2026-08-17" } },
            "select": [ "id", "amount", "customerⱵemail",
                        { "alias": "chased", "formula": "remindersↃinvoice.length" } ],
            "order": [ { "field": "due", "dir": "asc" } ],
            "limit": 50,
            "offset": 0
        })
    );
}

/// A host that answers a cursor plan the way [`sc_api`]'s does: `count` rows
/// numbered from 1, one batch at a time, resuming after the cursor it last
/// answered with. Nothing here knows what a table is — which is the point of
/// testing the streaming *protocol* on this side of the seam.
fn paging_host(count: i64) -> Arc<FakeHost> {
    FakeHost::new(move |plan: &Json| {
        let batch = plan["limit"].as_i64().unwrap_or(1000);
        let from = match plan["after"].as_array() {
            Some(after) => after[0].as_i64().unwrap() + 1,
            None => 1,
        } + plan["offset"].as_i64().unwrap_or(0);
        let ids: Vec<i64> = (from..=count).take(batch as usize).collect();
        let rows: Vec<Json> = ids.iter().map(|id| json!({ "id": id })).collect();
        let short = (rows.len() as i64) < batch;
        Ok(json!({
            "rows": rows,
            "cursor": match (short, ids.last()) {
                (false, Some(last)) => json!([last]),
                _ => Json::Null,
            },
        }))
    })
}

#[tokio::test]
async fn iter_streams_one_batch_at_a_time_and_resumes_from_the_cursor() {
    let host = paging_host(5);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"const seen = [];
                   for await (const row of db.invoices.where({ paid: false }).iter(2)) {
                     seen.push(row.id);
                   }
                   return seen;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!([1, 2, 3, 4, 5]), "every row, once, in order");

    let plans = host.plans();
    assert_eq!(
        plans.len(),
        3,
        "two full batches and the short one that ends it: {plans:#?}"
    );
    assert_eq!(
        plans[0],
        json!({
            "op": "select",
            "table": "invoices",
            "authority": "default",
            "where": { "paid": false },
            "cursor": true,
            "limit": 2,
        }),
        "the first batch carries the whole query and no cursor"
    );
    // Each later batch is the same plan, resumed — the filter rides along,
    // because a batch is a read of its own.
    assert_eq!(plans[1]["after"], json!([2]));
    assert_eq!(plans[1]["where"], json!({ "paid": false }));
    assert_eq!(plans[2]["after"], json!([4]));
}

#[tokio::test]
async fn iter_fetches_nothing_until_it_is_asked_and_stops_when_the_loop_does() {
    let host = paging_host(1000);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"const it = db.invoices.iter(10);
                   const before = await db.invoices.count();
                   let first = null;
                   for await (const row of it) { first = row.id; break; }
                   return { first: first, before: before };"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out["first"], json!(1));

    let plans = host.plans();
    assert_eq!(
        plans.len(),
        2,
        "the `.count()` ran before the iterator's first batch, so nothing was \
             fetched at `.iter()` itself: {plans:#?}"
    );
    assert_eq!(plans[0]["op"], json!("aggregate"), "the count went first");
    assert_eq!(
        plans[1]["cursor"],
        json!(true),
        "and the loop's first batch second — the only one, because it broke"
    );
}

#[tokio::test]
async fn a_limit_bounds_the_iteration_and_the_batch_never_overshoots_it() {
    let host = paging_host(1000);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"const seen = [];
                   for await (const row of db.invoices.limit(3).iter(2)) seen.push(row.id);
                   return seen;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!([1, 2, 3]),
        "the limit is the total, not the batch"
    );
    let plans = host.plans();
    let limits: Vec<&Json> = plans.iter().map(|p| &p["limit"]).collect();
    assert_eq!(
        limits,
        vec![&json!(2), &json!(1)],
        "the last batch asks for what is left rather than a batch of it"
    );
}

#[tokio::test]
async fn iter_refuses_what_it_cannot_stream_in_front_of_the_author() {
    let host = paging_host(10);
    let rt = CodeRuntime::new();
    async fn refused(rt: &CodeRuntime, host: &dyn CodeHost, code: &str) -> String {
        rt.run(with_host(code, host)).await.unwrap_err().to_string()
    }
    let refused = |code: &'static str| refused(&rt, &*host, code);
    let e = refused(
        "for await (const g of db.invoices.groupBy('paid').aggregate({ n: 'count()' }).iter()) {}",
    )
    .await;
    assert!(e.contains("aggregates"), "{e}");
    let e = refused("for await (const r of db.invoices.iter(0)) {}").await;
    assert!(e.contains("how many rows to read at a time"), "{e}");
    let e = refused("for await (const r of db.invoices.iter('lots')) {}").await;
    assert!(e.contains("how many rows to read at a time"), "{e}");
}

#[tokio::test]
async fn the_two_spellings_of_a_filter_and_repeated_wheres_and() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    rt.run(with_host(
        r#"await db.books.where({ status: "draft" }).where('pages > 3').rows();
               await db.table("books").where('status === "draft"').rows();
               return null;"#,
        &*host,
    ))
    .await
    .unwrap();
    let plans = host.plans();
    assert_eq!(
        plans[0]["where"],
        json!({ "and": [ { "status": "draft" }, { "formula": "pages > 3" } ] })
    );
    // `db.table(name)` is the general form of the `db.name` sugar.
    assert_eq!(plans[1]["table"], json!("books"));
    assert_eq!(
        plans[1]["where"],
        json!({ "formula": "status === \"draft\"" })
    );
}

#[tokio::test]
async fn authority_is_the_runs_default_until_delegated_and_where_it_is_said_does_not_matter() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    rt.run(with_host(
        r#"await db.invoices.rows();
               await db.asUser().invoices.rows();
               await db.invoices.asUser().where({ paid: false }).rows();
               await db.invoices.where({ paid: false }).asUser().rows();
               await db.asUser().invoices.asAdmin().rows();
               return null;"#,
        &*host,
    ))
    .await
    .unwrap();
    let plans = host.plans();
    let authority: Vec<&str> = plans
        .iter()
        .filter_map(|p| p["authority"].as_str())
        .collect();
    assert_eq!(
        authority,
        vec!["default", "user", "user", "user", "admin"],
        "asUser() sets one field of the plan, wherever it is said"
    );
}

#[tokio::test]
async fn the_bodys_own_sql_is_a_request_of_its_own_and_says_whose_authority_it_runs_under() {
    let host = FakeHost::rows(json!([{ "n": 3 }]));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"const a = await db.sql("select count(*) as n from books where pages > $1", [200]);
                   await db.sql("select 1", [], { asUser: true });
                   await db.asUser().sql("select 1");
                   await db.asUser().sql("select 1", null, { asUser: false });
                   return a[0].n;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!(3), "the rows come back as they are");

    let plans = host.plans();
    assert_eq!(
        plans.len(),
        4,
        "one call is one round trip, as a terminal is"
    );
    assert_eq!(
        plans[0],
        json!({
            "op": "sql",
            "authority": "default",
            "sql": "select count(*) as n from books where pages > $1",
            "params": [200],
        }),
        "the text is the body's and the values ride beside it"
    );
    let authority: Vec<&str> = plans
        .iter()
        .filter_map(|p| p["authority"].as_str())
        .collect();
    assert_eq!(
        authority,
        vec!["default", "user", "user", "admin"],
        "the option and the handle say the same thing, and the option wins"
    );
    assert_eq!(
        plans[2]["params"],
        json!([]),
        "no arguments is an empty list"
    );
}

#[tokio::test]
async fn a_malformed_sql_call_is_refused_before_anything_is_sent() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    // Each of these is a body that meant something the host would have to
    // guess at, so the prelude says what it takes instead — and says it
    // without a round trip.
    for (code, expected) in [
        (r#"return await db.sql({ from: "books" });"#, "SQL text"),
        (
            r#"return await db.sql("select 1", { id: 1 });"#,
            "array of values",
        ),
        (
            r#"return await db.sql("select 1", [], { asuser: true });"#,
            "asuser",
        ),
    ] {
        let refused = rt
            .run(with_host(code, &*host))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains(expected), "{code}: {refused}");
    }
    assert!(
        host.plans().is_empty(),
        "nothing reached the host: each was refused in the guest"
    );
}

#[tokio::test]
async fn the_terminals_carry_their_own_op_and_unwrap_their_own_result() {
    let host = FakeHost::new(|plan| {
        Ok(match plan["op"].as_str() {
            Some("aggregate") => json!({ "value": 12 }),
            Some("insert") => json!({ "id": 9 }),
            Some("update") => json!({ "updated": 2, "ids": [3, 7] }),
            Some("delete") => json!({ "deleted": 1, "ids": [7] }),
            _ => json!([{ "id": 4 }]),
        })
    });
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"return {
                     first:  await db.books.first(),
                     get:    await db.books.get(4),
                     exists: await db.books.where({ id: 4 }).exists(),
                     count:  await db.books.count(),
                     sum:    await db.books.sum("qty * price"),
                     insert: await db.books.insert({ title: "Orlando" }),
                     update: await db.books.where({ id: 3 }).update({ shelf: 3 }),
                     del:    await db.books.where({ id: 7 }).delete(),
                   };"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out["first"], json!({ "id": 4 }));
    assert_eq!(out["get"], json!({ "id": 4 }));
    assert_eq!(out["exists"], json!(true));
    assert_eq!(out["count"], json!(12));
    assert_eq!(out["sum"], json!(12));
    assert_eq!(out["update"], json!({ "updated": 2, "ids": [3, 7] }));
    assert_eq!(out["del"], json!({ "deleted": 1, "ids": [7] }));

    let plans = host.plans();
    let ops: Vec<&str> = plans.iter().filter_map(|p| p["op"].as_str()).collect();
    assert_eq!(
        ops,
        vec![
            "select",
            "select",
            "select",
            "aggregate",
            "aggregate",
            "insert",
            "update",
            "delete"
        ]
    );
    assert_eq!(plans[0]["limit"], json!(1), ".first() is LIMIT 1");
    assert_eq!(plans[1]["pk"], json!(4), ".get(pk) names the key");
    assert_eq!(
        plans[3]["aggregate"],
        json!([{ "alias": "value", "fn": "count", "arg": null }])
    );
    assert_eq!(
        plans[4]["aggregate"],
        json!([{ "alias": "value", "fn": "sum", "arg": "qty * price" }])
    );
    assert_eq!(plans[5]["values"], json!({ "title": "Orlando" }));
}

#[tokio::test]
async fn a_grouped_aggregate_is_one_plan_and_answers_rows() {
    // The phase 6 chain: the group keys, the values written the way the
    // formula language writes an aggregate, a bound on the groups, and rows
    // out — one round trip, like every other terminal.
    let host = FakeHost::new(|_| Ok(json!([{ "author": 1, "n": 3, "total": "42" }])));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_host(
            r#"return await db.books
                     .where({ shelf: 2 })
                     .groupBy("author")
                     .aggregate({ n: "count()", total: "sum(price * qty)" })
                     .having({ n: { gt: 2 } })
                     .orderBy("n", "desc")
                     .limit(10)
                     .rows();"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!([{ "author": 1, "n": 3, "total": "42" }]));
    let plans = host.plans();
    assert_eq!(plans.len(), 1, "one terminal is one round trip");
    assert_eq!(
        plans[0],
        json!({
            "op": "aggregate",
            "table": "books",
            "authority": "default",
            "where": { "shelf": 2 },
            "group": ["author"],
            "having": { "n": { "gt": 2 } },
            "aggregate": [
                { "alias": "n", "fn": "count", "arg": null },
                { "alias": "total", "fn": "sum", "arg": "price * qty" },
            ],
            "order": [ { "field": "n", "dir": "desc" } ],
            "limit": 10
        })
    );

    // Ungrouped, `.aggregate({…}).rows()` is the same plan without a `group`,
    // and the host's one object reads back as the one row it is.
    let host = FakeHost::new(|_| Ok(json!({ "n": 7 })));
    let out = rt
        .run(with_host(
            r#"return await db.books.aggregate({ n: "count()" }).rows();"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!([{ "n": 7 }]));
}

#[tokio::test]
async fn a_group_without_values_and_a_scalar_terminal_over_groups_are_both_refused() {
    // Neither reaches the host: a group with nothing to compute is a
    // question with no answer, and `.count()` over many groups has no one
    // value to be.
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    for (body, wanted) in [
        (
            r#"return await db.books.groupBy("author").rows();"#,
            "needs an .aggregate(",
        ),
        (
            r#"return await db.books.groupBy("author").count();"#,
            "answers one value",
        ),
        (
            r#"return await db.books.aggregate({ n: "count" }).rows();"#,
            "is not an aggregate",
        ),
    ] {
        let err = rt
            .run(with_host(body, &*host))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(wanted), "{err}");
    }
    assert!(host.plans().is_empty(), "nothing reached the host");
}

#[tokio::test]
async fn an_unfiltered_update_or_delete_is_refused_before_it_is_sent() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    for body in [
        "return await db.books.update({ shelf: 3 });",
        "return await db.books.delete();",
    ] {
        let err = rt
            .run(with_host(body, &*host))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("without a .where()"), "{err}");
    }
    assert!(host.plans().is_empty(), "nothing reached the host");
}

#[tokio::test]
async fn a_host_error_is_thrown_into_the_body_and_is_catchable() {
    let host = FakeHost::new(|_| Err(Error::auth("the ownership formula does not grant this")));
    let rt = CodeRuntime::new();
    // Uncaught, it fails the run with the host's own message.
    let err = rt
        .run(with_host("return await db.books.rows();", &*host))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ownership formula does not grant"), "{err}");
    // Caught, the body carries on — §5's "try a delegated write and fall back".
    let out = rt
        .run(with_host(
            "try { await db.books.asUser().rows(); } catch (e) { return e.message; } return null;",
            &*host,
        ))
        .await
        .unwrap();
    assert!(
        out.as_str()
            .is_some_and(|m| m.contains("the ownership formula does not grant this")),
        "{out}"
    );
}

#[tokio::test]
async fn the_call_budget_is_a_named_error() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    let mut c = with_host(
        "for (let i = 0; i < 100; i++) await db.books.rows(); return true;",
        &*host,
    );
    c.max_calls = 3;
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("more than 3 database calls"), "{err}");
    assert_eq!(host.plans().len(), 3, "the budget is spent, not exceeded");
}

#[tokio::test]
async fn the_pool_serves_two_bodies_at_once() {
    // Two workers, two bodies each blocking its thread on a slow host: if
    // they were serialised the pair would take twice one call.
    let slow = Arc::new(FakeHost {
        plans: Mutex::new(Vec::new()),
        answer: Box::new(|_| Ok(json!([]))),
        delay: Some(Duration::from_millis(300)),
        calls: AtomicU32::new(0),
    });
    let rt = Arc::new(CodeRuntime::with_workers(2));
    let started = Instant::now();
    // The host is borrowed, so each task owns its own `Arc` and lends it.
    let one = {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
        tokio::spawn(async move { rt.run(with_host("return await db.a.rows();", &*host)).await })
    };
    let two = {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
        tokio::spawn(async move { rt.run(with_host("return await db.b.rows();", &*host)).await })
    };
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(550),
        "the two runs serialised: {:?}",
        started.elapsed()
    );
    assert_eq!(slow.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_body_that_spins_is_terminated_and_its_isolate_recovers() {
    let rt = CodeRuntime::with_workers(1);
    // A slice is never longer than what is left of the run's own wall clock,
    // so a body given 100 ms is stopped at 100 ms — by the clock that ran out
    // first, and in that clock's own words.
    let mut c = call("while (true) {}");
    c.timeout = Some(Duration::from_millis(100));
    let started = Instant::now();
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("exceeded its 100 ms time limit"), "{err}");
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "it was not the watchdog that stopped it: {:?}",
        started.elapsed()
    );
    // The same worker serves the next run normally.
    assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
}

#[tokio::test]
async fn a_body_that_never_yields_is_stopped_at_the_slice_not_at_its_timeout() {
    // The second clock. This body has thirty seconds of wall clock and uses
    // none of it in the database, so nothing about its *timeout* is what
    // ought to stop it: what it is doing wrong is holding the isolate — and
    // every other body resident on it — without yielding.
    let rt = CodeRuntime::with_workers(1);
    let mut c = call("while (true) {}");
    c.timeout = Some(Duration::from_secs(30));
    let started = Instant::now();
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("without awaiting anything"), "{err}");
    assert!(
        err.contains(&format!("{} ms", DEFAULT_JS_SLICE.as_millis())),
        "the slice is not named: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it waited for the wall clock: {:?}",
        started.elapsed()
    );
    // And a body that spins *after* an await is the same body: the slice is a
    // fresh window at each resumption, not a budget the run spends.
    let host = FakeHost::rows(json!([]));
    let mut c = with_host("await db.books.rows(); while (true) {}", &*host);
    c.timeout = Some(Duration::from_secs(30));
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("without awaiting anything"), "{err}");
    assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
}

#[tokio::test]
async fn time_in_the_host_does_not_count_against_the_js_watchdog_but_does_against_the_deadline() {
    // Each call sleeps for a third of the run's whole budget. Three of them
    // outlast the deadline — but the *watchdog* must not fire, because the
    // guest's own JavaScript has run for microseconds.
    let slow = Arc::new(FakeHost {
        plans: Mutex::new(Vec::new()),
        answer: Box::new(|_| Ok(json!([]))),
        delay: Some(Duration::from_millis(120)),
        calls: AtomicU32::new(0),
    });
    let rt = CodeRuntime::with_workers(1);
    let mut c = with_host(
        "for (let i = 0; i < 10; i++) await db.books.rows(); return true;",
        &*slow,
    );
    c.timeout = Some(Duration::from_millis(300));
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(
        err.contains("time limit") && !err.contains("timed out"),
        "the host's time was charged to the guest's watchdog: {err}"
    );
    // A body that only sleeps in the host, well inside the deadline, is fine
    // even though one host call alone exceeds a formula's whole timeout.
    let mut c = with_host("await db.books.rows(); return true;", &*slow);
    c.timeout = Some(Duration::from_millis(1000));
    assert_eq!(rt.run(c).await.unwrap(), json!(true));
}

#[tokio::test]
async fn the_globals_cannot_be_poisoned_for_the_next_run() {
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::with_workers(1);
    // A body that tries to replace the op handle, the run wrapper, the `db`
    // factory and `db` itself — the last two of which are shared by every
    // body on this isolate now that neither is compiled per run.
    let out = rt
        .run(with_host(
            r#"let broke = [];
                   try { globalThis.__scDbCall = () => []; } catch (e) { broke.push("call"); }
                   try { delete globalThis.__scInvoke; } catch (e) { broke.push("invoke"); }
                   try { globalThis.__scMakeDb = () => ({}); } catch (e) { broke.push("makeDb"); }
                   try { Object.defineProperty(globalThis, "__scDbCall", { value: 1 }); }
                     catch (e) { broke.push("define"); }
                   globalThis.db = "poisoned";
                   db = "poisoned for this run only";
                   return broke;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!(["call", "invoke", "makeDb", "define"]),
        "strict mode throws"
    );
    // The next run on the same isolate gets its own `db` and a working op —
    // the handle is the factory's answer to *this* run's token, so what the
    // last body did to its own binding went with it.
    let out = rt
        .run(with_host("return await db.books.rows();", &*host))
        .await
        .unwrap();
    assert_eq!(out, json!([{ "id": 1 }]));
}

#[tokio::test]
async fn the_code_isolate_has_no_io_surface_of_its_own() {
    let rt = CodeRuntime::new();
    for probe in ["Deno", "fetch", "process", "setTimeout"] {
        let out = rt
            .run(call(&format!("return typeof {probe} === 'undefined';")))
            .await
            .unwrap();
        assert_eq!(out, json!(true), "sandbox leak: {probe}");
    }
    // `require` is the one name of that family that exists, and it reaches
    // nothing either: its whole body is a refusal (see [`REQUIRE`]), so a
    // body that calls it is told what to write instead of being told that a
    // function is not one.
    let err = rt
        .run(call(r#"return require("node:fs");"#))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`node:fs`"), "{err}");
    assert!(err.contains("not available in a code body"), "{err}");
    // The op handle exists — that is the one surface — but a body cannot
    // name its own run to it: the token is the `db` factory's argument and
    // stays in the handle's closure, so what a body holds is the handle.
    let out = rt
        .run(call("return typeof __scTok === 'undefined';"))
        .await
        .unwrap();
    assert_eq!(out, json!(true), "the run token is in the guest's scope");
    // The factory is a global like the op, so a body can ask it for a handle
    // over any token it likes — and get one that names nothing, which is
    // what the 128 random bits are for.
    let err = rt
        .run(call(
            r#"return await __scMakeDb("00000000000000000000000000000000").books.rows();"#,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("has already finished"), "{err}");
    // And the same by hand, at the op: a token that is not this run's names
    // nothing, because the table is keyed by those bits precisely so that a
    // body cannot reach another resident run's host by guessing at it.
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let err = rt
        .run(with_host(
            r#"return await __scDbCall("00000000000000000000000000000000",
                     { op: "select", table: "books" });"#,
            &*host,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("has already finished"), "{err}");
    assert!(host.plans().is_empty(), "nothing reached the host");
}

#[tokio::test]
async fn a_returned_promise_is_awaited_rather_than_refused() {
    // The inversion this milestone turns on: the run wrapper used to refuse
    // a Promise because there was nothing in the sandbox to await it with.
    let rt = CodeRuntime::new();
    assert_eq!(
        rt.run(call("return (async () => 1)();")).await.unwrap(),
        json!(1)
    );
    // And `await` is legal at the top level of a body, because the body is
    // the inside of an async function.
    assert_eq!(
        rt.run(call("const n = await Promise.resolve(2); return n + 1;"))
            .await
            .unwrap(),
        json!(3)
    );
    // A rejection is the run's failure, with the thrown message.
    let err = rt
        .run(call("await Promise.reject(new Error('nope'));"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("nope"), "{err}");
}

#[tokio::test]
async fn a_forgotten_await_is_one_named_error_rather_than_a_wrong_answer() {
    // Each of these is what a missing `await` used to look like: `{}` out of
    // JSON.stringify, `[object Promise]` out of a template string, and a bare
    // "is not iterable" TypeError out of a `for … of`. All three now name the
    // mistake, and name it at the point the body made it.
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::new();
    for body in [
        "return JSON.stringify(db.books.rows());",
        "return `${db.books.count()}`;",
        "for (const r of db.books.rows()) {} return null;",
        "return db.books.count() + 1;",
    ] {
        let err = rt
            .run(with_host(body, &*host))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("was not awaited"), "{body}: {err}");
    }
    // Returning one *is* awaiting it, though — `return db.books.rows()` from
    // an async body resolves to the rows, which is what its author meant.
    // The error is for the uses that would otherwise answer something else.
    assert_eq!(
        rt.run(with_host("return db.books.rows();", &*host))
            .await
            .unwrap(),
        json!([{ "id": 1 }])
    );
    // The chain itself is untouched: only a terminal answers a promise, so
    // `await` belongs at the end and nowhere inside.
    let out = rt
        .run(with_host(
            "return await db.books.where({ id: 1 }).orderBy('id').rows();",
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!([{ "id": 1 }]));
}

#[tokio::test]
async fn promise_all_issues_its_queries_without_waiting_for_each() {
    // `Promise.all` is the spelling §1 promises works, and it is only
    // meaningful because a host call suspends rather than blocks: both plans
    // reach the host before either answer comes back.
    let host = FakeHost::new(|plan| {
        Ok(match plan["table"].as_str() {
            Some("books") => json!([{ "id": 1 }]),
            _ => json!([{ "id": 2 }]),
        })
    });
    let out = CodeRuntime::new()
        .run(with_host(
            r#"const [books, shelves] = await Promise.all([
                     db.books.rows(),
                     db.shelves.rows(),
                   ]);
                   return { books: books, shelves: shelves };"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out["books"], json!([{ "id": 1 }]));
    assert_eq!(out["shelves"], json!([{ "id": 2 }]));
    assert_eq!(host.plans().len(), 2);
}

#[tokio::test]
async fn a_binding_that_collides_with_the_handle_is_refused() {
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::new();
    let mut c = with_host("return 1;", &*host);
    c.bindings.insert("db".into(), json!(1));
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("collides with the table handle"), "{err}");
    // With no host there is no handle, so the name is the caller's to use.
    let mut c = call("return db;");
    c.bindings.insert("db".into(), json!(1));
    assert_eq!(rt.run(c).await.unwrap(), json!(1));
}

#[tokio::test]
async fn an_expired_run_leaves_nothing_behind_for_the_next_one() {
    // A run abandoned at its wall clock is suspended *inside* the isolate,
    // and its continuation would otherwise resume during the next run's event
    // loop — reaching that run's host, spending that run's call budget, and
    // issuing queries in a stranger's name. So it is unwound before the
    // worker takes another job.
    let hung = Arc::new(FakeHost {
        plans: Mutex::new(Vec::new()),
        answer: Box::new(|_| Ok(json!([]))),
        delay: Some(Duration::from_secs(30)),
        calls: AtomicU32::new(0),
    });
    let rt = CodeRuntime::with_workers(1);
    let mut expiring = with_host(
        "await db.a.rows(); await db.a.rows(); return 'never';",
        &*hung,
    );
    expiring.timeout = Some(Duration::from_millis(150));
    let err = rt.run(expiring).await.unwrap_err().to_string();
    assert!(err.contains("time limit"), "{err}");

    // The next run on the same isolate is whole: its own host, and its own
    // budget, neither spent by what the last one left behind.
    let next = FakeHost::rows(json!([{ "id": 1 }]));
    let mut c = with_host(
        "let n = 0; for (let i = 0; i < 3; i++) n += (await db.b.rows()).length; return n;",
        &*next,
    );
    c.max_calls = 3;
    assert_eq!(rt.run(c).await.unwrap(), json!(3));
    assert_eq!(
        next.plans().len(),
        3,
        "the abandoned run spent none of this one's budget"
    );
    assert_eq!(
        hung.calls.load(Ordering::SeqCst),
        1,
        "and reached none of its host either"
    );
}

#[tokio::test]
async fn a_body_suspended_on_a_promise_that_never_settles_loses_the_run_not_the_worker() {
    // The one shape the isolate's own two bounds cannot see: no JavaScript is
    // running, so the watchdog has nothing to terminate, and no host call is
    // in flight, so the deadline check is never reached. The **event loop**
    // sees it — it empties with the run's promise still pending — so the run
    // fails at once, by name, rather than holding the worker to its deadline.
    let rt = CodeRuntime::with_workers(1);
    let mut c = call("await new Promise(() => {}); return 1;");
    c.timeout = Some(Duration::from_secs(30));
    let started = Instant::now();
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(
        err.contains("awaited something that never happens"),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it waited it out"
    );
    // The same worker serves the next run normally.
    assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
}

#[tokio::test]
async fn a_run_no_longer_waits_for_a_worker_and_a_hung_call_still_ends_at_its_deadline() {
    // One worker, already serving a body whose host call will not come back
    // for half a minute. Before this milestone that worker was *held* — a run
    // cost a thread — and a second run could not start until the first
    // finished, which is the deadlock a body whose write fires a second
    // trigger fell into. Now the first run is a pending promise, so the
    // second is admitted at once and answers in microseconds.
    let slow = Arc::new(FakeHost {
        plans: Mutex::new(Vec::new()),
        answer: Box::new(|_| Ok(json!([]))),
        delay: Some(Duration::from_secs(30)),
        calls: AtomicU32::new(0),
    });
    let rt = Arc::new(CodeRuntime::with_workers(1));
    let blocked = {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
        tokio::spawn(async move {
            let mut c = with_host("return await db.a.rows();", &*host);
            c.timeout = Some(Duration::from_secs(2));
            rt.run(c).await
        })
    };
    // Let the first run reach its host call.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut queued = call("return 1;");
    queued.timeout = Some(Duration::from_millis(200));
    let started = Instant::now();
    assert_eq!(
        rt.run(queued).await.unwrap(),
        json!(1),
        "a run behind a suspended one is not a run behind a blocked thread"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "it waited for the resident run: {:?}",
        started.elapsed()
    );

    // And the run that *is* suspended is still bounded, rather than holding
    // its caller for as long as the database takes.
    let err = blocked
        .await
        .unwrap()
        .expect_err("a host call that never returns is not a run without a bound")
        .to_string();
    assert!(err.contains("time limit"), "{err}");
}

/// A host that answers after `delay`, counting how many calls are in flight
/// at the same time and remembering the most there ever were.
///
/// The number is the milestone: it is what tells "hundreds of runs served at
/// once" apart from "hundreds of runs served one after another quickly".
struct CountingHost {
    delay: Duration,
    live: Mutex<usize>,
    peak: AtomicUsize,
    answer: Json,
}

impl CountingHost {
    fn new(delay: Duration, answer: Json) -> Arc<CountingHost> {
        Arc::new(CountingHost {
            delay,
            live: Mutex::new(0),
            peak: AtomicUsize::new(0),
            answer,
        })
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CodeHost for CountingHost {
    async fn call(&self, _request: Json) -> Result<Json> {
        {
            let mut live = self.live.lock().unwrap();
            *live += 1;
            self.peak.fetch_max(*live, Ordering::SeqCst);
        }
        tokio::time::sleep(self.delay).await;
        *self.live.lock().unwrap() -= 1;
        Ok(self.answer.clone())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_worker_serves_two_hundred_runs_at_once() {
    // The milestone, on a **single** isolate: 200 bodies, each a read and a
    // write, against a host that takes 50 ms per call. Serialised that is
    // 200 × 2 × 50 ms = 20 seconds; concurrent it is two round trips and the
    // isolate's own overhead. The peak in-flight count is the assertion that
    // matters — a fast wall clock could be a fast host, but a hundred calls
    // outstanding at one time can only be concurrency.
    const RUNS: usize = 200;
    let host = CountingHost::new(Duration::from_millis(50), json!([{ "id": 1 }]));
    let rt = Arc::new(CodeRuntime::with_workers(1));
    let started = Instant::now();
    let mut runs = Vec::new();
    for n in 0..RUNS {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        runs.push(tokio::spawn(async move {
            let mut c = with_host(
                "const rows = await db.books.where({ id: 1 }).rows();
                     await db.log.insert({ n: rows.length });
                     return rows.length;",
                &*host,
            );
            c.bindings.insert("which".into(), json!(n));
            c.timeout = Some(Duration::from_secs(20));
            rt.run(c).await
        }));
    }
    for run in runs {
        assert_eq!(run.await.unwrap().unwrap(), json!(1));
    }
    let elapsed = started.elapsed();
    assert!(
        host.peak() >= 100,
        "the runs were served one at a time: {} in flight at the peak",
        host.peak()
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "200 runs of two 50 ms calls took {elapsed:?}, which is the shape of a queue"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resident_runs_do_not_leak_into_each_other() {
    // Fifty runs on one isolate, each with its own bindings and its own
    // budget. The token is what keeps them apart: a run's `db` closes over
    // it, the host it reaches is the one its own table entry holds, and the
    // calls it spends come out of its own count.
    const RUNS: u32 = 50;
    let host = CountingHost::new(Duration::from_millis(20), json!([{ "id": 1 }]));
    let rt = Arc::new(CodeRuntime::with_workers(1));
    let mut runs = Vec::new();
    for n in 0..RUNS {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        runs.push(tokio::spawn(async move {
            // One run in five spends its whole budget in a loop; the rest
            // ask for exactly what they were given and must get it back.
            let greedy = n % 5 == 0;
            let code = if greedy {
                "for (let i = 0; i < 100; i++) await db.books.rows(); return mine;"
            } else {
                "const rows = await db.books.rows();
                     if (rows.length !== 1) throw new Error('wrong host');
                     return mine;"
            };
            let mut c = with_host(code, &*host);
            c.bindings.insert("mine".into(), json!(n));
            c.max_calls = 3;
            c.timeout = Some(Duration::from_secs(20));
            (n, greedy, rt.run(c).await)
        }));
    }
    for run in runs {
        let (n, greedy, outcome) = run.await.unwrap();
        if greedy {
            let err = outcome.unwrap_err().to_string();
            assert!(err.contains("more than 3 database calls"), "{n}: {err}");
        } else {
            assert_eq!(
                outcome.unwrap(),
                json!(n),
                "run {n} answered another run's bindings"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_whose_write_fires_a_second_body_completes_on_one_worker() {
    // The deadlock this milestone removes, and the test that would have
    // failed before it. The outer body's `.insert()` raises a table event; a
    // `run_js_code` trigger on that event is a *second* code body, and it
    // needs the runtime while the first is still holding its place. With one
    // worker and one run per worker that could never finish — the pool size
    // was `1 + max nesting depth × concurrency`, which is not a number
    // anyone can pick. With runs resident it is a pending promise inside a
    // pending promise, and one isolate is enough.
    struct NestingHost {
        rt: Arc<CodeRuntime>,
        depth: AtomicU32,
    }

    #[async_trait]
    impl CodeHost for NestingHost {
        async fn call(&self, request: Json) -> Result<Json> {
            if request["op"] == json!("insert") && self.depth.fetch_add(1, Ordering::SeqCst) < 2 {
                // The trigger the write fired: another code body, on the
                // same pool, while this one is suspended waiting for us.
                let inner = self.rt.run(CodeCall {
                    code: "return await db.audit.insert({ what: 'nested' });".to_owned(),
                    host: Some(self),
                    timeout: Some(Duration::from_secs(5)),
                    ..CodeCall::default()
                });
                inner.await?;
            }
            Ok(json!({ "id": 1 }))
        }
    }

    let rt = Arc::new(CodeRuntime::with_workers(1));
    let host = Arc::new(NestingHost {
        rt: Arc::clone(&rt),
        depth: AtomicU32::new(0),
    });
    let out = rt
        .run(with_host(
            "return await db.invoices.insert({ paid: false });",
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!({ "id": 1 }));
    assert_eq!(
        host.depth.load(Ordering::SeqCst),
        3,
        "the nested bodies did not all run"
    );
}

#[tokio::test]
async fn a_finished_run_gives_its_place_back() {
    // The occupancy count is what the dispatcher chooses between workers by,
    // and what the admission bound is measured against. A run that ends the
    // ordinary way — a completion op, from inside a poll of the event loop —
    // has to decrement it just as a reaped or terminated one does, or the
    // count only ever grows and both of those decisions rot.
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::with_workers(1);
    let place = || rt.workers[0].outstanding.load(Ordering::SeqCst);
    for _ in 0..5 {
        rt.run(with_host("return await db.books.rows();", &*host))
            .await
            .unwrap();
    }
    assert_eq!(place(), 0, "five ordinary runs");
    // A body that throws is a run that ended too.
    rt.run(with_host("throw new Error('no');", &*host))
        .await
        .unwrap_err();
    assert_eq!(place(), 0, "a body that threw");
    // So is one refused before its script was ever built.
    let mut bad = with_host("return 1;", &*host);
    bad.bindings.insert("db".into(), json!(1));
    rt.run(bad).await.unwrap_err();
    assert_eq!(place(), 0, "a run refused before it started");
    // And so is one the watchdog took out.
    let mut spinning = call("while (true) {}");
    spinning.timeout = Some(Duration::from_millis(100));
    rt.run(spinning).await.unwrap_err();
    assert_eq!(place(), 0, "a run that was terminated");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_freed_place_is_filled_while_its_neighbour_is_still_in_flight() {
    // Two places, one long body and ten short ones. A run finishes *inside* a
    // poll of the event loop, and that poll does not return while the long
    // body is still in flight — so unless finishing a run says so, the second
    // place would stay shut until the long body finished and emptied the loop,
    // and every short run would be queued behind a body it has nothing to do
    // with. Which is why the assertion is the interleaving and not the clock:
    // all ten shorts answer *while* the long one is still going.
    let host = CountingHost::new(Duration::from_millis(40), json!([]));
    let rt = Arc::new(CodeRuntime::with_workers_and_inflight(1, 2));
    let long = {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        tokio::spawn(async move {
            let mut c = with_host(
                "for (let i = 0; i < 20; i++) await db.books.rows(); return 'long';",
                &*host,
            );
            c.timeout = Some(Duration::from_secs(20));
            rt.run(c).await
        })
    };
    // Let the long body take one of the two places.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut shorts = Vec::new();
    for _ in 0..10 {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        shorts.push(tokio::spawn(async move {
            let mut c = with_host("return (await db.books.rows()).length;", &*host);
            c.timeout = Some(Duration::from_secs(20));
            rt.run(c).await
        }));
    }
    for short in shorts {
        assert_eq!(short.await.unwrap().unwrap(), json!(0));
    }
    assert!(
        !long.is_finished(),
        "the ten short runs waited for the long one to give the place back"
    );
    assert_eq!(long.await.unwrap().unwrap(), json!("long"));
    assert!(
        host.peak() <= 2,
        "the bound admitted more than it was given: {}",
        host.peak()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_occupancy_bound_queues_the_overflow_rather_than_refusing_it() {
    // A worker admits `max_inflight` runs; the rest wait in the channel, and
    // their wall clock is running while they do — which is why the queue is a
    // queue and not a second, hidden, unbounded resource.
    let host = CountingHost::new(Duration::from_millis(50), json!([]));
    let rt = Arc::new(CodeRuntime::with_workers_and_inflight(1, 4));
    let mut runs = Vec::new();
    for _ in 0..12 {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        runs.push(tokio::spawn(async move {
            let mut c = with_host("return (await db.books.rows()).length;", &*host);
            c.timeout = Some(Duration::from_secs(10));
            rt.run(c).await
        }));
    }
    for run in runs {
        assert_eq!(run.await.unwrap().unwrap(), json!(0));
    }
    assert!(
        host.peak() <= 4,
        "the bound admitted more than it was given: {}",
        host.peak()
    );
}

/// A host that records what it was asked to do and holds every call for
/// `delay`, so that a run can be left provably suspended — with a write
/// behind it — while something else happens on its isolate.
struct WriteCounter {
    writes: AtomicU32,
    delay: Duration,
}

#[async_trait]
impl CodeHost for WriteCounter {
    async fn call(&self, request: Json) -> Result<Json> {
        if request["op"] == json!("insert") {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        tokio::time::sleep(self.delay).await;
        Ok(json!({ "id": 1 }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runaway_is_blamed_and_its_co_residents_are_never_re_run() {
    // The termination is the blunt instrument: it stops the isolate, not the
    // body that overran it. So three runs share one isolate and the
    // difference between them is the whole of this phase.
    //
    //   W  has written, and is suspended waiting for the host. Stopping it
    //      costs its caller an error; **re-running** it would cost a second
    //      row, so it is answered rather than retried.
    //   C  has made no host call at all — it is resident because its own
    //      completion is a microtask the next poll will drain — so there is
    //      provably nothing to repeat, and it goes back in the queue.
    //   B  is the body that never yields, and the one the message is for.
    let host = Arc::new(WriteCounter {
        writes: AtomicU32::new(0),
        delay: Duration::from_millis(800),
    });
    let rt = Arc::new(CodeRuntime::with_workers(1));

    let writer = {
        let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
        tokio::spawn(async move {
            let mut c = with_host("await db.log.insert({ n: 1 }); return 'W';", &*host);
            c.timeout = Some(Duration::from_secs(20));
            rt.run(c).await
        })
    };
    // Long enough for W to be resident and suspended in its insert.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both submitted from one task, so both are in the worker's channel
    // before it wakes: it admits C, and admits B from the same burst without
    // a poll in between — which is why C is still resident, and still
    // callless, when B stops the isolate.
    let mut spinner = call("while (true) {}");
    spinner.timeout = Some(Duration::from_secs(20));
    let (innocent, guilty) = tokio::join!(rt.run(call("return 'C';")), rt.run(spinner));

    let blamed = guilty
        .expect_err("a body that never yields is not a body that ran")
        .to_string();
    assert!(
        blamed.contains("without awaiting anything"),
        "the runaway was not the one blamed: {blamed}"
    );
    assert_eq!(
        innocent.expect("a co-resident with no host call is owed another go"),
        json!("C"),
        "the callless co-resident was not re-run"
    );
    let bystander = writer
        .await
        .unwrap()
        .expect_err("the writer went down with the isolate")
        .to_string();
    assert!(
        bystander.contains("another code body on the same isolate"),
        "the writer was told it was its own fault: {bystander}"
    );
    assert!(
        bystander.contains("re-running it could repeat"),
        "the message does not say why it was not retried: {bystander}"
    );
    assert_eq!(
        host.writes.load(Ordering::SeqCst),
        1,
        "the body that had already written was run a second time"
    );
    // The isolate is whole afterwards.
    assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_loops_on_microtasks_loses_its_run_and_not_its_worker() {
    // The shape no op can see: a body that yields, but only to the microtask
    // queue, so nothing marks a resumption and nothing ever returns to Rust.
    // The mark left standing from the run's own admission is what keeps the
    // watchdog on it.
    let rt = CodeRuntime::with_workers(1);
    let mut c = call("for (;;) { await null; }");
    c.timeout = Some(Duration::from_secs(20));
    let started = Instant::now();
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("without awaiting anything"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the worker was held to the wall clock: {:?}",
        started.elapsed()
    );
    assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_heap_bound_stops_admitting_rather_than_the_process() {
    // A body that keeps everything it reads, on an isolate given 24 MB to
    // keep it in. V8's own answer to a heap limit is to abort the process;
    // this is the answer instead — the near-heap-limit callback buys a grace
    // and stops admission, and a body that fills even the grace is stopped
    // the way any other runaway is.
    //
    // The body awaits between allocations, so its JS slice is fresh every
    // time round: what stops it can only be the heap.
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::build(1, 8, 24 * 1024 * 1024);
    let mut hog = with_host(
        "const kept = [];
             for (let i = 0; i < 100; i++) {
               await db.books.rows();
               kept.push(new Array(200000).fill('x'));
             }
             return kept.length;",
        &*host,
    );
    hog.timeout = Some(Duration::from_secs(30));
    hog.max_calls = 1000;
    let err = rt.run(hog).await.unwrap_err().to_string();
    assert!(
        err.contains("more memory than the JavaScript engine has"),
        "{err}"
    );
    // And the isolate is still serving: the process is here to be asked.
    assert_eq!(
        rt.run(with_host("return (await db.books.rows()).length;", &*host))
            .await
            .unwrap(),
        json!(1)
    );
}

/// The body a run of `code` would be compiled from.
fn definition_of(code: &str, bindings: &[(&str, Json)]) -> RunScripts {
    let mut run = CodeRun {
        code: code.to_owned(),
        bindings: BTreeMap::new(),
        host: None,
        fetch: None,
        files: None,
        file_stores: Vec::new(),
        triggers: None,
        trigger_names: Vec::new(),
        module_fns: None,
        module_functions: Vec::new(),
        live: None,
        timeout: Duration::from_secs(1),
        max_calls: 10,
        max_fetches: 10,
        max_file_ops: 10,
        max_trigger_runs: 10,
        max_module_calls: 10,
        libraries: None,
        ratelimit_scope: "instance".to_owned(),
        schema: None,
        console: None,
        trace: None,
        started: None,
    };
    for (name, value) in bindings {
        run.bindings.insert((*name).to_owned(), value.clone());
    }
    build_run_scripts(&run).unwrap()
}

/// A snapshot with one table in it, at a given generation.
fn snapshot_of(generation: u64, table: &str) -> SchemaSnapshot {
    SchemaSnapshot::new(
        generation,
        format!(r#"{{"tables":[{{"name":"{table}","fields":[]}}]}}"#),
    )
}

#[tokio::test]
async fn the_schema_reaches_the_isolate_and_travels_only_on_a_miss() {
    // One worker, so all three runs land on one isolate — which is what
    // makes "the isolate already has it" a thing this test can observe.
    let rt = CodeRuntime::with_workers(1);
    let fifth = snapshot_of(5, "books");

    let mut first = call("return globalThis.__scSchema().tables[0].name;");
    first.schema = Some(&fifth);
    assert_eq!(rt.run(first).await.unwrap(), json!("books"));

    // A second run at the same generation: the isolate keeps what it was
    // given, and the guest reads the same schema. That the JSON did not
    // travel a second time is `build_script`'s claim, asserted below.
    let mut again = call("return globalThis.__scSchema().tables[0].name;");
    again.schema = Some(&fifth);
    assert_eq!(rt.run(again).await.unwrap(), json!("books"));

    // A reload bumps the generation, and the new snapshot replaces the old
    // one on the isolate.
    let sixth = snapshot_of(6, "authors");
    let mut after = call("return globalThis.__scSchema().tables[0].name;");
    after.schema = Some(&sixth);
    assert_eq!(rt.run(after).await.unwrap(), json!("authors"));

    // A run given no snapshot names none: `__scSchema` with no generation
    // still answers the one this isolate holds, but the run's own is null —
    // which is what Phase 2's `Table` will be absent for.
    assert_eq!(
        rt.run(call("return 1;")).await.unwrap(),
        json!(1),
        "a run with no schema is an ordinary run"
    );
}

#[tokio::test]
async fn a_generation_the_isolate_does_not_hold_fails_by_name() {
    // Unreachable while Rust and the isolate agree — the define always
    // precedes the invoke that names it — and named rather than silent
    // anyway, because the alternative is a `Table` that knows no tables and
    // says nothing about why. This is the same lookup `__scInvoke` makes,
    // asked for a generation nothing defined.
    let rt = CodeRuntime::with_workers(1);
    let held = snapshot_of(5, "books");
    let mut warm = call("return 1;");
    warm.schema = Some(&held);
    rt.run(warm).await.unwrap();

    let refusal = rt
        .run(call("return globalThis.__scSchema(9).tables.length;"))
        .await
        .expect_err("a schema this isolate has not got is not an empty schema")
        .to_string();
    assert!(refusal.contains("generation 9"), "{refusal}");
    assert!(refusal.contains("not on this isolate"), "{refusal}");
}

#[test]
fn the_schema_travels_only_when_the_isolate_has_not_got_that_generation() {
    let scripts = definition_of("return 1;", &[]);
    let key = BodyCache::key(&scripts.definition);
    let fifth = snapshot_of(5, "books");

    // A miss carries the whole snapshot — as a *string literal* to be
    // parsed, so a table named `");alert(1);//` is a name and not a token.
    let miss = build_script(
        &scripts,
        "aa",
        key,
        false,
        "[]",
        "[]",
        "[]",
        "{}",
        Some(&fifth),
        Some(5),
    );
    assert!(miss.contains("__scDefineSchema(5, \""), "{miss}");
    assert!(miss.contains("books"), "{miss}");
    // The generation, then the console's method names, which are the last
    // argument every invocation carries.
    assert!(
        miss.contains(", 5, [\"log\""),
        "the generation travels too: {miss}"
    );

    // A hit carries one integer and no JSON at all — the whole reason a
    // generation exists rather than a hash of the schema.
    let hit = build_script(
        &scripts,
        "bb",
        key,
        true,
        "[]",
        "[]",
        "[]",
        "{}",
        None,
        Some(5),
    );
    assert!(!hit.contains("__scDefineSchema"), "{hit}");
    assert!(!hit.contains("books"), "the schema travelled: {hit}");
    assert!(hit.contains(", 5, [\"log\""), "{hit}");

    // And a run with no snapshot at all names none, so the guest resolves
    // nothing rather than the last schema some other run left behind.
    let none = build_script(
        &scripts, "cc", key, true, "[]", "[]", "[]", "{}", None, None,
    );
    assert!(none.contains(", null, [\"log\""), "{none}");
}

#[test]
fn a_body_travels_only_on_a_miss() {
    // The hot path's whole claim: once the isolate has the body, what a run
    // sends is a token, a key and its own bindings — and the source is not
    // in it anywhere.
    let mut cache = BodyCache::new();
    let scripts = definition_of("return secret + 1;", &[("secret", json!(41))]);
    let key = BodyCache::key(&scripts.definition);
    assert!(
        !cache.holds(key, &scripts.definition),
        "nothing is warm yet"
    );

    let miss = build_script(
        &scripts, "aa", key, false, "[]", "[]", "[]", "{}", None, None,
    );
    assert!(miss.contains("__scDefine"), "{miss}");
    assert!(miss.contains("return secret + 1;"), "{miss}");
    assert!(miss.contains("__scInvoke"), "{miss}");
    assert!(cache.store(key, scripts.definition.clone()).is_empty());

    assert!(
        cache.holds(key, &scripts.definition),
        "the isolate has it now"
    );
    let hit = build_script(
        &scripts, "bb", key, true, "[]", "[]", "[]", "{}", None, None,
    );
    assert!(!hit.contains("__scDefine"), "{hit}");
    assert!(
        !hit.contains("return secret + 1;"),
        "the source travelled: {hit}"
    );
    // The bindings still do, because they are what differs per run.
    assert!(hit.contains("41"), "{hit}");
    assert!(hit.contains("bb"), "{hit}");

    // The *same* code with different binding names is a different body, and
    // gets its own key: the `const`s are part of what was compiled.
    let renamed = definition_of("return secret + 1;", &[("other", json!(41))]);
    assert!(!cache.holds(BodyCache::key(&renamed.definition), &renamed.definition));
}

#[test]
fn a_key_collision_costs_a_compile_and_never_the_wrong_body() {
    // The key is a cheap hash, so it is the *definition* beside it that
    // decides a hit. Two sources under one key is a miss, and defining
    // replaces what the isolate had rather than shadowing it.
    let mut cache = BodyCache::new();
    let first = definition_of("return 1;", &[]);
    let key = BodyCache::key(&first.definition);
    cache.store(key, first.definition.clone());
    let second = definition_of("return 2;", &[]);
    assert!(
        !cache.holds(key, &second.definition),
        "a different body under the same key must not be served from it"
    );
    cache.store(key, second.definition.clone());
    assert!(cache.holds(key, &second.definition));
    assert!(!cache.holds(key, &first.definition));
}

#[test]
fn the_cache_drops_the_least_recently_run_body() {
    // Bounded, because a server with many distinct bodies must not
    // accumulate compiled functions in an isolate for ever.
    let mut cache = BodyCache::new();
    let mut keys = Vec::new();
    for n in 0..BODY_CACHE_CAPACITY {
        let scripts = definition_of(&format!("return {n};"), &[]);
        let key = BodyCache::key(&scripts.definition);
        assert!(
            cache.store(key, scripts.definition).is_empty(),
            "no eviction yet"
        );
        keys.push(key);
    }
    // Touching the oldest is what makes the *second* oldest the one to go.
    let oldest = definition_of("return 0;", &[]);
    assert!(cache.holds(keys[0], &oldest.definition));

    let extra = definition_of("return 'one too many';", &[]);
    let evicted = cache.store(BodyCache::key(&extra.definition), extra.definition);
    assert_eq!(evicted, vec![keys[1]], "the least recently run body goes");
    assert!(
        cache.holds(keys[0], &oldest.definition),
        "the touched one stays"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_the_isolate_has_forgotten_is_compiled_again() {
    // The eviction path end to end: one body, then more distinct bodies than
    // the cache holds, then the first one again — which the isolate has
    // forgotten and must be sent afresh. A cache that lied here would be a
    // run that never answers.
    let rt = CodeRuntime::with_workers(1);
    assert_eq!(
        rt.run(call("return 'first';")).await.unwrap(),
        json!("first")
    );
    for n in 0..=BODY_CACHE_CAPACITY {
        assert_eq!(
            rt.run(call(&format!("return {n};"))).await.unwrap(),
            json!(n),
            "distinct body {n}"
        );
    }
    assert_eq!(
        rt.run(call("return 'first';")).await.unwrap(),
        json!("first")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_compiled_body_hands_each_run_its_own_db() {
    // Decision 5, now kept by the factory rather than by recompiling the
    // prelude: the body is compiled once, and `db` is an argument built per
    // run from that run's token. So a body that mutates its own `db` — the
    // only thing it can do to it — takes nothing with it into the next run
    // of the same compiled function, and each run reaches its own host.
    let mine = FakeHost::rows(json!([{ "id": 1 }]));
    let theirs = FakeHost::rows(json!([{ "id": 2 }, { "id": 3 }]));
    let rt = CodeRuntime::with_workers(1);
    let body = "const rows = await db.books.rows();
                    db = 'wrecked for this run';
                    return rows.length;";
    assert_eq!(rt.run(with_host(body, &*mine)).await.unwrap(), json!(1));
    // The same compiled body, a second run, a different host of its own.
    assert_eq!(rt.run(with_host(body, &*theirs)).await.unwrap(), json!(2));
    assert_eq!(mine.plans().len(), 1);
    assert_eq!(theirs.plans().len(), 1);
}

/// Phase 4's benchmark: how many code bodies **one isolate** runs per
/// second.
///
/// One body, run over and over against a host that answers at once, so what
/// is timed is the runtime's own overhead — what a body costs to compile, to
/// admit, and to carry through the bridge and back — rather than a database.
/// It is what the [`BodyCache`] and `__scMakeDb` were measured with: about
/// 2,000 runs a second before them and about 44,000 after, on the machine
/// they were written on. The assertion is deliberately loose, because a
/// benchmark that fails on a busy machine is a flaky test; the number it
/// *prints* is what the CHANGELOG records.
#[tokio::test(flavor = "multi_thread")]
async fn throughput_of_one_isolate() {
    const RUNS: usize = 2000;
    const CONCURRENCY: usize = 32;
    let host = FakeHost::rows(json!([{ "id": 1, "title": "Dune" }]));
    let rt = CodeRuntime::with_workers(1);
    let body = r#"const rows = await db.books
                        .where({ id: payload.id })
                        .limit(1)
                        .rows();
                      return rows.length;"#;
    let one = |n: usize| {
        let mut c = with_host(body, &*host);
        c.bindings.insert("payload".into(), json!({ "id": n }));
        c.timeout = Some(Duration::from_secs(30));
        rt.run(c)
    };
    // Warm whatever there is to warm, so the number is the steady state.
    for n in 0..CONCURRENCY {
        assert_eq!(one(n).await.unwrap(), json!(1));
    }
    let started = Instant::now();
    for batch in 0..(RUNS / CONCURRENCY) {
        let outcomes = deno_core::futures::future::join_all(
            (0..CONCURRENCY).map(|n| one(batch * CONCURRENCY + n)),
        )
        .await;
        for outcome in outcomes {
            assert_eq!(outcome.unwrap(), json!(1));
        }
    }
    let elapsed = started.elapsed();
    let per_second = RUNS as f64 / elapsed.as_secs_f64();
    println!(
        "one isolate: {RUNS} runs in {elapsed:?} = {per_second:.0} runs/second \
             ({:.2} ms each)",
        elapsed.as_secs_f64() * 1000.0 / RUNS as f64
    );
    assert!(per_second > 50.0, "{per_second:.0} runs/second");
}

// -----------------------------------------------------------------------
// `fetch`: the second host surface
// -----------------------------------------------------------------------

type Answered = Box<dyn Fn(&Json) -> Result<Json> + Send + Sync>;

/// A network that records what it was asked to send and answers from a
/// closure — [`FakeHost`]'s counterpart, and the proof that the fetch seam
/// is JSON like the other one: no socket is opened anywhere in these tests.
struct FakeNet {
    sent: Mutex<Vec<Json>>,
    answer: Answered,
    delay: Option<Duration>,
    /// Requests in flight, and the most there have ever been at once —
    /// which is how "these two went out together" is asserted.
    live: AtomicU32,
    peak: AtomicU32,
}

impl FakeNet {
    fn new(answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static) -> Arc<FakeNet> {
        Arc::new(FakeNet {
            sent: Mutex::new(Vec::new()),
            answer: Box::new(answer),
            delay: None,
            live: AtomicU32::new(0),
            peak: AtomicU32::new(0),
        })
    }

    /// The everyday answer: 200, with this JSON as the body.
    fn ok(body: Json) -> Arc<FakeNet> {
        FakeNet::new(move |_| {
            Ok(json!({
                "status": 200,
                "status_text": "OK",
                "url": "https://api.example.com/thing",
                "redirected": false,
                "headers": [["content-type", "application/json"]],
                "text": body.to_string(),
            }))
        })
    }

    fn sent(&self) -> Vec<Json> {
        self.sent.lock().unwrap().clone()
    }

    fn peak(&self) -> u32 {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl FetchHost for FakeNet {
    async fn fetch(&self, request: Json) -> Result<Json> {
        self.sent.lock().unwrap().push(request.clone());
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
        (self.answer)(&request)
    }
}

/// A call with the network, and optionally the tables.
fn with_net<'a>(code: &str, net: &'a dyn FetchHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        fetch: Some(net),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_fetches_and_reads_the_response() {
    let net = FakeNet::ok(json!({ "id": 7, "name": "Ada" }));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_net(
            r#"const res = await fetch("https://api.example.com/thing");
                   return {
                     ok: res.ok,
                     status: res.status,
                     statusText: res.statusText,
                     url: res.url,
                     type: res.headers.get("content-type"),
                     missing: res.headers.get("x-nope"),
                     body: await res.json(),
                   };"#,
            &*net,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({
            "ok": true, "status": 200, "statusText": "OK",
            "url": "https://api.example.com/thing",
            "type": "application/json", "missing": Json::Null,
            "body": { "id": 7, "name": "Ada" },
        })
    );
    // And what went out is one plain JSON request: a GET, no body.
    let sent = net.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["url"], json!("https://api.example.com/thing"));
    assert_eq!(sent[0]["method"], json!("GET"));
    assert_eq!(sent[0]["body"], Json::Null);
}

#[tokio::test]
async fn a_post_sends_the_headers_and_body_the_body_built() {
    let net = FakeNet::ok(json!({ "ok": true }));
    let rt = CodeRuntime::new();
    rt.run(with_net(
            r#"await fetch("https://api.example.com/hooks", {
                 method: "post",
                 headers: { "Authorization": "Bearer t0ken" },
                 body: { id: 1, title: "Orlando" },
               });
               const h = new Headers([["x-a", "1"]]);
               h.append("x-a", "2");
               h.set("content-type", "text/csv");
               await fetch("https://api.example.com/csv", { method: "PUT", headers: h, body: "a,b" });
               return null;"#,
            &*net,
        ))
        .await
        .unwrap();
    let sent = net.sent();
    // The method is upper-cased, an object body is JSON (which the web
    // would have sent as `[object Object]`), and the content type it
    // implies is filled in without overwriting one the body set.
    assert_eq!(sent[0]["method"], json!("POST"));
    assert_eq!(sent[0]["body"], json!(r#"{"id":1,"title":"Orlando"}"#));
    assert_eq!(sent[0]["body_base64"], json!(false));
    let headers = |plan: &Json| -> Vec<(String, String)> {
        plan["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().to_owned(),
                    pair[1].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    assert_eq!(
        headers(&sent[0]),
        vec![
            ("authorization".to_owned(), "Bearer t0ken".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ]
    );
    // A repeated header stays repeated on the wire; `set` replaced the one
    // the string body would otherwise have implied.
    assert_eq!(
        headers(&sent[1]),
        vec![
            ("x-a".to_owned(), "1".to_owned()),
            ("x-a".to_owned(), "2".to_owned()),
            ("content-type".to_owned(), "text/csv".to_owned()),
        ]
    );
    assert_eq!(sent[1]["body"], json!("a,b"));
}

#[tokio::test]
async fn a_status_the_server_did_not_like_is_not_an_error() {
    // The web API's rule, and the one people are surprised by in the other
    // direction: only a transport failure rejects. A 404 is an answer.
    let net = FakeNet::new(|_| {
        Ok(json!({
            "status": 404, "status_text": "Not Found",
            "url": "https://api.example.com/gone", "redirected": true,
            "headers": [], "text": "no such thing",
        }))
    });
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_net(
            r#"const res = await fetch("https://api.example.com/gone");
                   return { ok: res.ok, status: res.status, redirected: res.redirected,
                            text: await res.text() };"#,
            &*net,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({ "ok": false, "status": 404, "redirected": true, "text": "no such thing" })
    );
}

#[tokio::test]
async fn a_transport_failure_is_a_type_error_the_body_can_catch() {
    let net = FakeNet::new(|_| Err(Error::msg("connection refused")));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_net(
            r#"try {
                     await fetch("https://nowhere.invalid/");
                     return "no throw";
                   } catch (e) {
                     return { name: e.constructor.name, message: e.message };
                   }"#,
            &*net,
        ))
        .await
        .unwrap();
    assert_eq!(out["name"], json!("TypeError"), "{out}");
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("connection refused"),
        "{out}"
    );
}

#[tokio::test]
async fn a_body_without_the_network_cannot_name_fetch() {
    // The capability is the parameter: no fetch host, no `fetch` in scope —
    // a ReferenceError naming it, exactly as `db` is for a pure body.
    let rt = CodeRuntime::new();
    let err = rt
        .run(call(r#"return await fetch("https://example.com/");"#))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("fetch is not defined"), "{err}");
    // A body with tables but no network is the same: one capability does
    // not carry the other.
    let host = FakeHost::rows(json!([]));
    let err = rt
        .run(with_host(
            r#"await db.books.rows(); return typeof fetch;"#,
            &*host,
        ))
        .await
        .unwrap();
    assert_eq!(err, json!("undefined"));
}

#[tokio::test]
async fn the_fetch_budget_is_counted_apart_from_the_database_one() {
    let net = FakeNet::ok(json!({}));
    let host = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::new();
    let mut c = CodeCall {
        code: r#"let sent = 0;
                     try {
                       for (let i = 0; i < 10; i++) { await fetch("https://x.test/" + i); sent++; }
                     } catch (e) {
                       // The database is still there: the two budgets are two.
                       const rows = await db.books.rows();
                       return { sent: sent, rows: rows.length, why: e.message };
                     }
                     return { sent: sent };"#
            .to_owned(),
        host: Some(&*host),
        fetch: Some(&*net),
        ..CodeCall::default()
    };
    c.max_fetches = 3;
    let out = rt.run(c).await.unwrap();
    assert_eq!(out["sent"], json!(3), "{out}");
    assert_eq!(out["rows"], json!(1), "{out}");
    assert!(
        out["why"]
            .as_str()
            .unwrap()
            .contains("more than 3 fetch requests"),
        "{out}"
    );
    assert_eq!(net.sent().len(), 3);
}

#[tokio::test]
async fn a_forgotten_await_on_a_fetch_says_so() {
    let net = FakeNet::ok(json!({ "id": 1 }));
    let rt = CodeRuntime::new();
    let err = rt
        .run(with_net(
            r#"return { res: fetch("https://api.example.com/thing") };"#,
            &*net,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("this fetch was not awaited"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_fetches_in_one_promise_all_go_out_together() {
    // The bridge is one channel serving both surfaces, so a body that asks
    // for a query and two requests at once gets all three in flight — the
    // parallelism it wrote, not three round trips in a row.
    let net = Arc::new(FakeNet {
        sent: Mutex::new(Vec::new()),
        answer: Box::new(|_| {
            Ok(json!({
                "status": 200, "status_text": "OK", "url": "https://x.test/",
                "redirected": false, "headers": [], "text": "{}",
            }))
        }),
        delay: Some(Duration::from_millis(50)),
        live: AtomicU32::new(0),
        peak: AtomicU32::new(0),
    });
    let rt = CodeRuntime::new();
    let mut c = with_net(
        r#"const [a, b] = await Promise.all([
                 fetch("https://x.test/a"),
                 fetch("https://x.test/b"),
               ]);
               return [a.status, b.status];"#,
        &*net,
    );
    c.timeout = Some(Duration::from_secs(10));
    let started = Instant::now();
    assert_eq!(rt.run(c).await.unwrap(), json!([200, 200]));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(140),
        "the two requests were serialised: {elapsed:?}"
    );
    assert!(net.peak() >= 2, "never two at once");
}

#[tokio::test]
async fn a_request_may_not_outlive_the_run_that_made_it() {
    // Whatever the body asks for, what reaches the host is bounded by what
    // is left of the run's own wall clock — so a `fetch` cannot hold the
    // trigger's caller past the timeout it was configured with.
    let net = FakeNet::ok(json!({}));
    let rt = CodeRuntime::new();
    let mut c = with_net(
        r#"await fetch("https://x.test/a", { timeout_ms: 60000 });
               await fetch("https://x.test/b");
               return null;"#,
        &*net,
    );
    c.timeout = Some(Duration::from_millis(900));
    rt.run(c).await.unwrap();
    let sent = net.sent();
    let asked = sent[0]["timeout_ms"].as_u64().unwrap();
    assert!(asked <= 900, "a minute was allowed through: {asked}");
    // The default is likewise what is left rather than the ten seconds a
    // request gets when there is room for them.
    assert!(sent[1]["timeout_ms"].as_u64().unwrap() <= 900);
}

#[tokio::test]
async fn bytes_survive_the_seam_in_both_directions() {
    // Not text: the seam is JSON, so a body that is not valid UTF-8 travels
    // base64 — and neither codec is the isolate's, because there is no
    // `TextEncoder` in the sandbox to lend one.
    let net = FakeNet::new(|request| {
        assert_eq!(request["body_base64"], json!(true), "{request}");
        // Echo what was sent, as bytes.
        Ok(json!({
            "status": 200, "status_text": "OK", "url": "https://x.test/",
            "redirected": false, "headers": [],
            "base64": request["body"],
        }))
    });
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_net(
            r#"const sent = new Uint8Array([0, 159, 146, 150, 255]);
                   const res = await fetch("https://x.test/", { method: "POST", body: sent });
                   const got = await res.bytes();
                   return Array.from(got);"#,
            &*net,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!([0, 159, 146, 150, 255]));
    // A text body comes back as bytes too, encoded as UTF-8 by the guest.
    let net = FakeNet::ok(json!("héllo"));
    let out = rt
        .run(with_net(
            r#"const res = await fetch("https://x.test/");
                   return Array.from(new Uint8Array(await res.arrayBuffer()));"#,
            &*net,
        ))
        .await
        .unwrap();
    // `"héllo"` as JSON text: the quotes are part of it, and é is two bytes.
    assert_eq!(out, json!([34, 104, 195, 169, 108, 108, 111, 34]));
}

#[tokio::test]
async fn the_options_a_browser_needs_are_ignored_and_a_typo_is_not() {
    let net = FakeNet::ok(json!({}));
    let rt = CodeRuntime::new();
    // What a browser needs and a server does not is accepted, so code that
    // carries it runs unchanged.
    rt.run(with_net(
        r#"await fetch("https://x.test/", { mode: "cors", credentials: "omit", cache: "no-store" });
               return null;"#,
        &*net,
    ))
    .await
    .unwrap();
    // Everything else is refused **by name**: a misspelled `header` that
    // silently sent nothing is the failure this exists to prevent.
    for (code, expected) in [
        (
            r#"await fetch("https://x.test/", { header: { a: "b" } });"#,
            "`header` is not an option",
        ),
        (
            r#"await fetch("https://x.test/", { signal: {} });"#,
            "no `signal`",
        ),
        (
            r#"await fetch("https://x.test/", { redirect: "manual" });"#,
            "only follows redirects",
        ),
        (
            r#"await fetch("https://x.test/", { method: "GET", body: "x" });"#,
            "cannot carry a body",
        ),
        (r#"await fetch("");"#, "absolute http(s) URL"),
        (
            r#"await fetch("https://x.test/", { method: "TRACE" });"#,
            "is not a method",
        ),
    ] {
        let err = rt.run(with_net(code, &*net)).await.unwrap_err().to_string();
        assert!(err.contains(expected), "{code}\n{err}");
    }
}

#[tokio::test]
async fn a_response_body_is_read_once_and_cloned_to_read_twice() {
    let net = FakeNet::ok(json!({ "n": 1 }));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_net(
            r#"const res = await fetch("https://x.test/");
                   const copy = res.clone();
                   const first = await res.json();
                   let second = null;
                   try { await res.text(); } catch (e) { second = e.message; }
                   return { first: first, used: res.bodyUsed, second: second,
                            clone: await copy.text() };"#,
            &*net,
        ))
        .await
        .unwrap();
    assert_eq!(out["first"], json!({ "n": 1 }));
    assert_eq!(out["used"], json!(true));
    assert!(
        out["second"]
            .as_str()
            .unwrap()
            .contains("already been read"),
        "{out}"
    );
    assert_eq!(out["clone"], json!(r#"{"n":1}"#));
}

// -----------------------------------------------------------------------
// `fs`: the third host surface
// -----------------------------------------------------------------------

/// A file store that lives in a map — [`FakeNet`]'s counterpart, and the
/// same proof: the file seam is JSON, so nothing here touches a disk.
///
/// It implements enough of the operations to round-trip (a write is
/// readable, a copy copies, a listing lists) because the thing under test is
/// the *prelude*: what plan each method builds, and what it makes of the
/// answer.
struct FakeFiles {
    asked: Mutex<Vec<Json>>,
    /// `store`, `path` → the bytes, with directories implied by the paths.
    files: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    stores: Vec<String>,
}

impl FakeFiles {
    fn new(stores: &[&str]) -> Arc<FakeFiles> {
        Arc::new(FakeFiles {
            asked: Mutex::new(Vec::new()),
            files: Mutex::new(BTreeMap::new()),
            stores: stores.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// Put a file there before the body runs.
    fn put(self: &Arc<FakeFiles>, store: &str, path: &str, body: &str) -> Arc<FakeFiles> {
        self.files.lock().unwrap().insert(
            (store.to_owned(), path.to_owned()),
            body.as_bytes().to_vec(),
        );
        Arc::clone(self)
    }

    fn asked(&self) -> Vec<Json> {
        self.asked.lock().unwrap().clone()
    }

    fn read(&self, store: &str, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .get(&(store.to_owned(), path.to_owned()))
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// What one operation answers, in the shapes the prelude reads.
    fn answer(&self, request: &Json) -> Result<Json> {
        let text = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
        let (store, path, op) = (text("store"), text("path"), text("op"));
        let key = (store.clone(), path.clone());
        let mut files = self.files.lock().unwrap();
        // A directory is not stored; it exists when something is under it.
        let is_dir = |files: &BTreeMap<(String, String), Vec<u8>>, path: &str| {
            path.is_empty()
                || files
                    .keys()
                    .any(|(s, p)| *s == store && p.starts_with(&format!("{path}/")))
        };
        match op.as_str() {
            "read" => {
                let bytes = files.get(&key).cloned().ok_or_else(|| {
                    Error::not_found(format!("{path:?} does not exist in file store {store}"))
                })?;
                if text("encoding") == "base64" {
                    Ok(json!({ "base64": BASE64.encode(&bytes) }))
                } else {
                    Ok(json!({ "text": String::from_utf8_lossy(&bytes) }))
                }
            }
            "write" => {
                let bytes = match (request["text"].as_str(), request["base64"].as_str()) {
                    (Some(text), _) => text.as_bytes().to_vec(),
                    (None, Some(encoded)) => BASE64.decode(encoded).unwrap(),
                    _ => return Err(Error::msg("a write carries text or base64")),
                };
                if request["overwrite"] != json!(true) && files.contains_key(&key) {
                    return Err(Error::invalid(format!(
                        "{path:?} already exists in file store {store}"
                    )));
                }
                let written = bytes.len();
                files.insert(key, bytes);
                Ok(json!({ "bytes": written }))
            }
            "stat" => {
                if let Some(bytes) = files.get(&key) {
                    Ok(json!({
                        "size": bytes.len(), "isDirectory": false,
                        "modified": "2026-08-21T09:00:00.000Z", "mimeType": "text/plain",
                    }))
                } else if is_dir(&files, &path) {
                    Ok(json!({
                        "size": 0, "isDirectory": true, "modified": null, "mimeType": null,
                    }))
                } else {
                    Ok(Json::Null)
                }
            }
            "list" => {
                let prefix = if path.is_empty() {
                    String::new()
                } else {
                    format!("{path}/")
                };
                let mut out: Vec<Json> = Vec::new();
                let mut seen: Vec<String> = Vec::new();
                for ((s, p), bytes) in files.iter() {
                    if *s != store || !p.starts_with(&prefix) {
                        continue;
                    }
                    let rest = &p[prefix.len()..];
                    match rest.split_once('/') {
                        Some((dir, _)) => {
                            if seen.iter().any(|s| s == dir) {
                                continue;
                            }
                            seen.push(dir.to_owned());
                            out.push(json!({
                                "name": dir, "path": format!("{prefix}{dir}"),
                                "isDirectory": true, "size": null, "modified": null,
                            }));
                        }
                        None => out.push(json!({
                            "name": rest, "path": p, "isDirectory": false,
                            "size": bytes.len(), "modified": "2026-08-21T09:00:00.000Z",
                        })),
                    }
                }
                Ok(Json::Array(out))
            }
            "delete" => Ok(Json::Bool(files.remove(&key).is_some())),
            "mkdir" => Ok(Json::Null),
            "copy" | "rename" => {
                let to_store = request["toStore"]
                    .as_str()
                    .map_or_else(|| store.clone(), str::to_owned);
                let to_path = text("toPath");
                let bytes = files.get(&key).cloned().ok_or_else(|| {
                    Error::not_found(format!("{path:?} does not exist in file store {store}"))
                })?;
                let moved = bytes.len();
                files.insert((to_store, to_path), bytes);
                if op == "rename" {
                    files.remove(&key);
                    return Ok(Json::Null);
                }
                Ok(json!({ "bytes": moved }))
            }
            "meta" => Ok(json!({
                "minRole": 40, "effectiveMinRole": 10, "attributes": { "origin": "test" },
            })),
            "setMeta" => Ok(Json::Null),
            other => Err(Error::invalid(format!("`{other}` is not a file operation"))),
        }
    }
}

#[async_trait]
impl FileHost for FakeFiles {
    async fn files(&self, request: Json) -> Result<Json> {
        self.asked.lock().unwrap().push(request.clone());
        self.answer(&request)
    }

    fn store_names(&self) -> Vec<String> {
        self.stores.clone()
    }
}

/// A call with the file stores and nothing else.
fn with_files<'a>(code: &str, files: &'a dyn FileHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        files: Some(files),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_opens_a_file_and_reads_it() {
    let files = FakeFiles::new(&["myFileStore"]).put("myFileStore", "the_file.txt", "hello");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const theFile = fs("myFileStore").open("the_file.txt");
                   const exists = await theFile.exists();
                   const theString = await theFile.text();
                   return {
                     exists: exists, theString: theString,
                     path: theFile.path, name: theFile.name,
                     store: theFile.store.name, parent: theFile.parent.path,
                     stores: fs.stores,
                   };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({
            "exists": true, "theString": "hello",
            "path": "the_file.txt", "name": "the_file.txt",
            "store": "myFileStore", "parent": "",
            "stores": ["myFileStore"],
        })
    );
    // Opening touched nothing: the two awaits are the two operations, and
    // each carries the store, the path and whose authority it runs under.
    let asked = files.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0]["op"], json!("stat"));
    assert_eq!(asked[1]["op"], json!("read"));
    assert_eq!(asked[1]["store"], json!("myFileStore"));
    assert_eq!(asked[1]["path"], json!("the_file.txt"));
    assert_eq!(asked[1]["encoding"], json!("text"));
    assert_eq!(asked[1]["authority"], json!("default"));
}

#[tokio::test]
async fn exists_is_a_question_and_a_missing_file_is_an_error_only_when_read() {
    let files = FakeFiles::new(&["s"]).put("s", "there/deep.txt", "x");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const store = fs("s");
                   let failed = null;
                   try { await store.open("gone.txt").text(); } catch (e) { failed = e.message; }
                   return {
                     missing: await store.open("gone.txt").exists(),
                     // A directory is not the file of that name.
                     dirAsFile: await store.open("there").exists(),
                     dir: await store.dir("there").exists(),
                     file: await store.open("there/deep.txt").exists(),
                     failed: failed,
                   };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["missing"], json!(false));
    assert_eq!(out["dirAsFile"], json!(false));
    assert_eq!(out["dir"], json!(true));
    assert_eq!(out["file"], json!(true));
    assert!(
        out["failed"].as_str().unwrap().contains("does not exist"),
        "{out}"
    );
}

#[tokio::test]
async fn writing_takes_a_string_bytes_a_value_or_another_file() {
    let files = FakeFiles::new(&["s"]).put("s", "source.txt", "from the source");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const store = fs("s");
                   const wrote = await store.open("notes/a.txt").write("hello");
                   await store.open("data.json").write({ rows: 12, ok: true });
                   await store.open("raw.bin").write(new Uint8Array([1, 2, 3]));
                   // A file: copied host-side, so the bytes never come in here.
                   const copied = await store.open("backup.txt").write(store.open("source.txt"));
                   let clobber = null;
                   try {
                     await store.open("notes/a.txt").create("again");
                   } catch (e) { clobber = e.message; }
                   return { wrote: wrote, copied: copied, clobber: clobber,
                            back: await store.open("data.json").json() };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["wrote"], json!(5));
    assert_eq!(out["copied"], json!(15));
    assert_eq!(out["back"], json!({ "rows": 12, "ok": true }));
    assert!(out["clobber"].as_str().unwrap().contains("already exists"));
    // An object was stored as JSON rather than as `[object Object]`, bytes
    // travelled base64, and `create` said so by asking not to overwrite.
    assert_eq!(files.read("s", "notes/a.txt").unwrap(), "hello");
    assert_eq!(
        files.read("s", "data.json").unwrap(),
        r#"{"rows":12,"ok":true}"#
    );
    assert_eq!(files.read("s", "backup.txt").unwrap(), "from the source");
    let asked = files.asked();
    let write = |n: usize| {
        asked
            .iter()
            .filter(|a| a["op"] == json!("write"))
            .nth(n)
            .unwrap()
            .clone()
    };
    assert_eq!(write(0)["overwrite"], json!(true));
    assert_eq!(write(2)["base64"], json!("AQID"));
    let create = asked
        .iter()
        .find(|a| a["overwrite"] == json!(false))
        .unwrap();
    assert_eq!(create["op"], json!("write"));
    // The copy is one operation naming both ends, not a read and a write.
    let copy = asked.iter().find(|a| a["op"] == json!("copy")).unwrap();
    assert_eq!(copy["path"], json!("source.txt"));
    assert_eq!(copy["toPath"], json!("backup.txt"));
}

#[tokio::test]
async fn bytes_cross_as_base64_both_ways() {
    let files = FakeFiles::new(&["s"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const file = fs("s").open("blob.bin");
                   await file.write(new Uint8Array([0, 1, 255, 128, 65]));
                   const back = await file.bytes();
                   const buffer = await fs("s").open("blob.bin").arrayBuffer();
                   return { back: Array.from(back), bytes: buffer.byteLength };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["back"], json!([0, 1, 255, 128, 65]));
    assert_eq!(out["bytes"], json!(5));
}

#[tokio::test]
async fn a_listing_answers_files_and_directories_to_act_on() {
    let files = FakeFiles::new(&["s"])
        .put("s", "docs/a.txt", "a")
        .put("s", "docs/b.txt", "bb")
        .put("s", "docs/sub/c.txt", "ccc");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const docs = fs("s").dir("docs");
                   const listed = await docs.list();
                   const names = listed.map((e) => e.name + (e.isDirectory ? "/" : ""));
                   // The entries are the same objects everything else takes.
                   let total = 0;
                   for (const entry of listed) {
                     if (!entry.isDirectory) total += (await entry.text()).length;
                   }
                   return { names: names, total: total,
                            root: docs.parent.path, up: fs("s").root.parent };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["names"], json!(["a.txt", "b.txt", "sub/"]));
    assert_eq!(out["total"], json!(3));
    assert_eq!(out["root"], json!(""));
    assert_eq!(out["up"], Json::Null, "the store root has no parent");
}

#[tokio::test]
async fn a_listed_entry_keeps_what_the_listing_said() {
    // TODO F1.5: name, isDirectory, size and modified, as properties and in
    // what `JSON.stringify` makes of a listing — and `stat()` still works
    // on either kind of entry.
    let files = FakeFiles::new(&["s"])
        .put("s", "a.txt", "abc")
        .put("s", "sub/c.txt", "c");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const listed = await fs("s").dir("").list();
                   const [file, dir] = listed;
                   const opened = fs("s").open("a.txt");
                   return {
                     json: JSON.parse(JSON.stringify(listed)),
                     props: [file.name, file.isDirectory, file.size, file.modified,
                             dir.name, dir.isDirectory, dir.size, dir.modified],
                     fileStat: await file.stat(),
                     dirStat: await dir.stat(),
                     opened: [opened.size === undefined, JSON.parse(JSON.stringify(opened))],
                   };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(
        out["json"],
        json!([
            { "store": "s", "path": "a.txt", "name": "a.txt", "isDirectory": false,
              "size": 3, "modified": "2026-08-21T09:00:00.000Z" },
            { "store": "s", "path": "sub", "name": "sub", "isDirectory": true,
              "size": null, "modified": null },
        ])
    );
    assert_eq!(
        out["props"],
        json!([
            "a.txt",
            false,
            3,
            "2026-08-21T09:00:00.000Z",
            "sub",
            true,
            null,
            null
        ])
    );
    assert_eq!(out["fileStat"]["size"], json!(3));
    assert_eq!(out["dirStat"]["isDirectory"], json!(true));
    // A handle that was opened rather than listed has looked at nothing.
    assert_eq!(
        out["opened"],
        json!([true, { "store": "s", "path": "a.txt", "name": "a.txt", "isDirectory": false }])
    );
}

#[tokio::test]
async fn moving_and_copying_answer_the_destination() {
    let files = FakeFiles::new(&["s", "archive"]).put("s", "in/report.txt", "quarterly");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const file = fs("s").open("in/report.txt");
                   const copy = await file.copyTo(fs("archive").open("2026/report.txt"));
                   const moved = await file.moveTo("done/report.txt");
                   return { copy: copy.store.name + ":" + copy.path,
                            moved: moved.path, text: await moved.text(),
                            gone: await file.exists() };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["copy"], json!("archive:2026/report.txt"));
    assert_eq!(out["moved"], json!("done/report.txt"));
    assert_eq!(out["text"], json!("quarterly"));
    assert_eq!(out["gone"], json!(false));
    assert_eq!(
        files.read("archive", "2026/report.txt").unwrap(),
        "quarterly"
    );
    // The cross-store copy named the other store; neither ever names an
    // absolute path.
    let asked = files.asked();
    let copy = asked.iter().find(|a| a["op"] == json!("copy")).unwrap();
    assert_eq!(copy["toStore"], json!("archive"));
    assert_eq!(copy["overwrite"], json!(false));
}

#[tokio::test]
async fn a_path_that_leaves_the_store_is_refused_before_it_is_sent() {
    let files = FakeFiles::new(&["s"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const store = fs("s");
                   const said = [];
                   for (const bad of ["../etc/passwd", "a/../../b", "/etc/passwd", ".."]) {
                     try { store.open(bad); said.push("allowed"); }
                     catch (e) { said.push(e.constructor.name); }
                   }
                   // Tidied, though: the quirks of joining a path are not
                   // allowed to change which file is meant.
                   return { said: said, tidied: store.open("a//b/./c.txt").path };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(
        out["said"],
        json!(["TypeError", "TypeError", "TypeError", "TypeError"])
    );
    assert_eq!(out["tidied"], json!("a/b/c.txt"));
    assert!(files.asked().is_empty(), "nothing reached the host");
}

#[tokio::test]
async fn an_unknown_store_is_named_at_once() {
    let files = FakeFiles::new(&["uploads", "assets"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"try { fs("uplods"); return "allowed"; } catch (e) { return e.message; }"#,
            &*files,
        ))
        .await
        .unwrap();
    let message = out.as_str().unwrap();
    assert!(
        message.contains("no file store named `uplods`"),
        "{message}"
    );
    assert!(message.contains("uploads, assets"), "{message}");
    assert!(files.asked().is_empty(), "the typo cost no round trip");
}

#[tokio::test]
async fn authority_travels_with_the_handle() {
    let files = FakeFiles::new(&["s"]).put("s", "a.txt", "x");
    let rt = CodeRuntime::new();
    rt.run(with_files(
        r#"await fs("s").open("a.txt").text();
               await fs("s").asUser().open("a.txt").text();
               await fs("s").asUser().asAdmin().open("a.txt").text();
               // The file keeps the authority it was opened with.
               const delegated = fs("s").asUser().open("a.txt");
               await delegated.parent.list();
               return null;"#,
        &*files,
    ))
    .await
    .unwrap();
    let said: Vec<String> = files
        .asked()
        .iter()
        .map(|a| a["authority"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(said, ["default", "user", "admin", "user"]);
}

#[tokio::test]
async fn metadata_reports_what_is_set_and_what_applies() {
    let files = FakeFiles::new(&["s"]).put("s", "a.txt", "x");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const file = fs("s").open("a.txt");
                   const meta = await file.meta();
                   await file.setMeta({ minRole: 40, attributes: { origin: "trigger" } });
                   let refused = null;
                   try { await file.setMeta({ minRole: 40, colour: "red" }); }
                   catch (e) { refused = e.message; }
                   return { meta: meta, refused: refused };"#,
            &*files,
        ))
        .await
        .unwrap();
    assert_eq!(out["meta"]["minRole"], json!(40));
    assert_eq!(out["meta"]["effectiveMinRole"], json!(10));
    assert!(
        out["refused"].as_str().unwrap().contains("`colour`"),
        "{out}"
    );
    let set = files
        .asked()
        .into_iter()
        .find(|a| a["op"] == json!("setMeta"))
        .unwrap();
    assert_eq!(set["minRole"], json!(40));
    assert_eq!(set["attributes"], json!({ "origin": "trigger" }));
}

#[tokio::test]
async fn the_file_budget_bounds_a_walk_and_is_its_own() {
    let files = FakeFiles::new(&["s"]);
    let rt = CodeRuntime::new();
    let mut call = with_files(
        r#"const store = fs("s");
               let done = 0;
               try {
                 for (let i = 0; i < 10; i++) { await store.open("f" + i).write("x"); done++; }
               } catch (e) { return { done: done, said: e.message }; }
               return { done: done, said: null };"#,
        &*files,
    );
    call.max_file_ops = 3;
    let out = rt.run(call).await.unwrap();
    assert_eq!(out["done"], json!(3));
    let said = out["said"].as_str().unwrap();
    assert!(said.contains("more than 3 file operations"), "{said}");
}

#[tokio::test]
async fn a_forgotten_await_is_a_named_error() {
    let files = FakeFiles::new(&["s"]).put("s", "a.txt", "hello");
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_files(
            r#"const file = fs("s").open("a.txt");
                   try { return "" + file.text(); } catch (e) { return e.message; }"#,
            &*files,
        ))
        .await
        .unwrap();
    assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
}

#[tokio::test]
async fn a_body_without_the_file_host_has_no_fs_in_scope() {
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            // `typeof` would answer "undefined" without throwing, which is
            // exactly the silent absence the parameter list avoids.
            code: r#"return fs("s");"#.to_owned(),
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    // Not `undefined`: a capability a body was not given is a name that does
    // not exist, exactly as `db` and `fetch` are.
    assert!(error.contains("fs is not defined"), "{error}");
}

#[tokio::test]
async fn the_four_surfaces_are_bound_together_and_named_apart() {
    let host = FakeHost::new(|_| Ok(json!([])));
    let net = FakeNet::ok(json!({ "ok": true }));
    let files = FakeFiles::new(&["s"]);
    let runs = FakeTriggers::new(&["nightly"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(CodeCall {
            code: r#"return [typeof db, typeof fetch, typeof fs, typeof trigger];"#.to_owned(),
            host: Some(&*host),
            fetch: Some(&*net),
            files: Some(&*files),
            triggers: Some(&*runs),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    assert_eq!(out, json!(["object", "function", "function", "function"]));
}

// -----------------------------------------------------------------------
// The fourth surface: other triggers
// -----------------------------------------------------------------------

/// A trigger host that records what it was asked to run and answers with the
/// payload it was given — enough to assert the plan a handle builds, which
/// is all this crate is responsible for.
struct FakeTriggers {
    asked: Mutex<Vec<Json>>,
    names: Vec<String>,
    answer: Answered,
    delay: Option<Duration>,
}

impl FakeTriggers {
    fn new(names: &[&str]) -> Arc<FakeTriggers> {
        FakeTriggers::answering(names, |request| Ok(request["payload"].clone()))
    }

    fn answering(
        names: &[&str],
        answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static,
    ) -> Arc<FakeTriggers> {
        Arc::new(FakeTriggers {
            asked: Mutex::new(Vec::new()),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            answer: Box::new(answer),
            delay: None,
        })
    }

    fn slow(names: &[&str], delay: Duration) -> Arc<FakeTriggers> {
        let mut host = FakeTriggers::answering(names, |_| Ok(Json::Null));
        Arc::get_mut(&mut host).unwrap().delay = Some(delay);
        host
    }

    fn asked(&self) -> Vec<Json> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl TriggerHost for FakeTriggers {
    async fn run(&self, request: Json) -> Result<Json> {
        self.asked.lock().unwrap().push(request.clone());
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        (self.answer)(&request)
    }

    fn trigger_names(&self) -> Vec<String> {
        self.names.clone()
    }
}

/// A call that can run other triggers and nothing else.
fn with_triggers<'a>(code: &str, triggers: &'a dyn TriggerHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        triggers: Some(triggers),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_runs_another_trigger_and_gets_its_result() {
    let runs = FakeTriggers::new(&["archive_done", "reindex"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_triggers(
            r#"const archived = await trigger("archive_done").run({ before: "2026-08-01" });
                   const nothing = await trigger("reindex").run();
                   return {
                     archived: archived, nothing: nothing,
                     name: trigger("reindex").name, names: trigger.names,
                   };"#,
            &*runs,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({
            "archived": { "before": "2026-08-01" },
            // Nothing passed is an empty payload rather than null, so
            // `payload.x` in the trigger that runs is undefined.
            "nothing": {},
            "name": "reindex",
            "names": ["archive_done", "reindex"],
        })
    );
    // What crossed is one plain plan per run: the name, the payload, and
    // whose authority — the trigger's own unless the body said otherwise.
    let asked = runs.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0]["trigger"], json!("archive_done"));
    assert_eq!(asked[0]["payload"], json!({ "before": "2026-08-01" }));
    assert_eq!(asked[0]["authority"], json!("default"));
    assert_eq!(asked[1]["trigger"], json!("reindex"));
}

#[tokio::test]
async fn authority_is_on_the_handle_and_the_default_is_the_runs_own() {
    let runs = FakeTriggers::new(&["send_invoice"]);
    let rt = CodeRuntime::new();
    rt.run(with_triggers(
        r#"const t = trigger("send_invoice");
               await t.asUser().run({ id: 1 });
               await t.run({ id: 2 });
               await t.asUser().asAdmin().run({ id: 3 });
               return null;"#,
        &*runs,
    ))
    .await
    .unwrap();
    let asked = runs.asked();
    let authorities: Vec<&Json> = asked.iter().map(|a| &a["authority"]).collect();
    assert_eq!(
        authorities,
        vec![&json!("user"), &json!("default"), &json!("admin")]
    );
    // `asUser()` answers a *new* handle: the one it came from is unchanged,
    // which is what makes `const t = trigger(…)` safe to reuse.
    assert_eq!(asked[1]["payload"], json!({ "id": 2 }));
}

#[tokio::test]
async fn a_name_this_run_does_not_have_is_refused_at_once() {
    let runs = FakeTriggers::new(&["archive_done"]);
    let rt = CodeRuntime::new();
    let error = rt
        .run(with_triggers(
            r#"return await trigger("archiv").run();"#,
            &*runs,
        ))
        .await
        .unwrap_err()
        .to_string();
    // Named where the typo is, and told what does exist — rather than
    // deferred to the dispatcher, which would say the same thing later.
    assert!(error.contains("no trigger named `archiv`"), "{error}");
    assert!(error.contains("archive_done"), "{error}");
    assert!(runs.asked().is_empty(), "nothing was run");
}

#[tokio::test]
async fn a_trigger_that_failed_is_an_error_the_body_can_catch() {
    let runs = FakeTriggers::answering(&["risky"], |_| {
        Err(Error::invalid("trigger `risky`: the endpoint said no"))
    });
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_triggers(
            r#"try {
                     await trigger("risky").run();
                     return "ran";
                   } catch (e) { return e.message; }"#,
            &*runs,
        ))
        .await
        .unwrap();
    assert!(
        out.as_str().unwrap().contains("the endpoint said no"),
        "{out}"
    );
}

#[tokio::test]
async fn the_run_budget_bounds_the_width_of_a_cascade() {
    let runs = FakeTriggers::new(&["nightly"]);
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            code: r#"for (let i = 0; i < 10; i++) await trigger("nightly").run({ i: i });
                         return "done";"#
                .to_owned(),
            triggers: Some(&*runs),
            max_trigger_runs: 3,
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("more than 3 other triggers"), "{error}");
    // Bounded where it says it is: three ran, the fourth did not.
    assert_eq!(runs.asked().len(), 3);
}

#[tokio::test]
async fn a_child_is_clamped_to_what_is_left_of_the_parents_clock() {
    let runs = FakeTriggers::new(&["nightly"]);
    let rt = CodeRuntime::new();
    rt.run(CodeCall {
        code: r#"return await trigger("nightly").run();"#.to_owned(),
        triggers: Some(&*runs),
        timeout: Some(Duration::from_millis(2000)),
        ..CodeCall::default()
    })
    .await
    .unwrap();
    // The op fills the clock in: what is left of the run, less the margin
    // that keeps a slow child failing *inside* the body.
    let asked = runs.asked();
    let ms = asked[0]["timeout_ms"].as_u64().unwrap();
    assert!(
        (1000..=2000 - TRIGGER_MARGIN.as_millis() as u64).contains(&ms),
        "{ms} ms"
    );
}

#[tokio::test]
async fn a_child_that_outlives_its_parent_is_the_parents_own_timeout() {
    // The host here ignores the clock it is handed, which is what a real one
    // must not do — the point being that the run is bounded anyway.
    let runs = FakeTriggers::slow(&["slow"], Duration::from_secs(5));
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            code: r#"return await trigger("slow").run();"#.to_owned(),
            triggers: Some(&*runs),
            timeout: Some(Duration::from_millis(300)),
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("300 ms time limit"), "{error}");
}

#[tokio::test]
async fn a_forgotten_await_on_a_trigger_run_says_so() {
    let runs = FakeTriggers::new(&["nightly"]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_triggers(
            r#"const pending = trigger("nightly").run();
                   try { return JSON.stringify({ ran: pending }); }
                   catch (e) { return e.message; }"#,
            &*runs,
        ))
        .await
        .unwrap();
    assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
}

#[tokio::test]
async fn a_body_without_the_trigger_host_cannot_name_it() {
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            code: r#"return await trigger("nightly").run();"#.to_owned(),
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("trigger is not defined"), "{error}");
}

// ---------------------------------------------------------------------
// The fifth surface: the modules' own functions
// ---------------------------------------------------------------------

/// A module-function host that records every plan and answers from a
/// closure, over a declared function list — the seam again: this crate can
/// be tested against `@saltcorn/markdown`'s shape without npm, a worker or
/// a module.
struct FakeModuleFns {
    asked: Mutex<Vec<Json>>,
    functions: Vec<ModuleFunction>,
    answer: Answer,
}

impl FakeModuleFns {
    /// A host supplying `(module, function)` pairs, answering every call
    /// with the arguments it was given.
    fn new(pairs: &[(&str, &str)]) -> Arc<FakeModuleFns> {
        Arc::new(FakeModuleFns {
            asked: Mutex::new(Vec::new()),
            functions: pairs
                .iter()
                .map(|(module, name)| ModuleFunction {
                    module: (*module).to_owned(),
                    name: (*name).to_owned(),
                    description: format!("{name}, from {module}"),
                    is_async: false,
                    arguments: Vec::new(),
                })
                .collect(),
            answer: Box::new(|plan| Ok(plan["args"].clone())),
        })
    }

    fn answering(
        self: Arc<FakeModuleFns>,
        answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static,
    ) -> Arc<FakeModuleFns> {
        Arc::new(FakeModuleFns {
            asked: Mutex::new(Vec::new()),
            functions: self.functions.clone(),
            answer: Box::new(answer),
        })
    }

    fn asked(&self) -> Vec<Json> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModuleFnHost for FakeModuleFns {
    async fn call(&self, request: Json) -> Result<Json> {
        self.asked.lock().unwrap().push(request.clone());
        (self.answer)(&request)
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        self.functions.clone()
    }
}

// --- The sixth surface: live updates ------------------------------------

/// A live host that records what it was asked to publish, refusing a stream
/// called `stopped` the way the hub refuses one that is not running.
#[derive(Default)]
struct FakeLive {
    published: Mutex<Vec<(String, Option<String>, Json)>>,
}

impl LiveHost for FakeLive {
    fn topics(&self, _stream: &str) -> Result<LiveTopics> {
        Ok(LiveTopics::Single)
    }

    fn publish(&self, stream: &str, topic: Option<String>, value: Json) -> Result<Option<String>> {
        if stream == "stopped" {
            return Err(Error::invalid(
                "stream `stopped` is not running on this server",
            ));
        }
        self.published
            .lock()
            .unwrap()
            .push((stream.to_owned(), topic.clone(), value));
        Ok(topic)
    }
}

#[tokio::test]
async fn a_loop_publishes_progress_in_order_and_a_refusal_is_catchable() {
    let live = Arc::new(FakeLive::default());
    let host: Arc<dyn LiveHost> = live.clone();
    let rt = CodeRuntime::with_workers(1);
    let out = rt
            .run(CodeCall {
                code: r#"for (let i = 1; i <= 10; i++) {
                           live.publish("job_status", user.id, { message: "step", progress: i / 10 });
                         }
                         live.publish("announcements", { text: "no topic" });
                         const numbered = live.publish("board_activity", 7, { n: 1 });
                         let refused = null;
                         try { live.publish("stopped", null, {}); } catch (e) { refused = e.message; }
                         return { refused, numbered, kind: typeof live.publish };"#
                    .to_owned(),
                bindings: [("user".to_owned(), json!({ "id": "u-1" }))]
                    .into_iter()
                    .collect(),
                live: Some(&host),
                ..CodeCall::default()
            })
            .await
            .unwrap();
    assert_eq!(out["kind"], json!("function"));
    assert_eq!(
        out["numbered"],
        json!("7"),
        "a number topic crosses as text"
    );
    assert!(
        out["refused"].as_str().unwrap().contains("not running"),
        "{out}"
    );
    let published = live.published.lock().unwrap().clone();
    assert_eq!(published.len(), 12);
    let progress: Vec<Json> = published[..10]
        .iter()
        .map(|(_, _, v)| v["progress"].clone())
        .collect();
    assert_eq!(progress[0], json!(0.1));
    assert_eq!(progress[9], json!(1), "in the order they were published");
    assert!(
        published[..10]
            .iter()
            .all(|(s, t, _)| s == "job_status" && t.as_deref() == Some("u-1"))
    );
    assert_eq!(published[10].0, "announcements");
    assert_eq!(published[10].1, None, "two arguments: no topic");
    assert_eq!(published[10].2, json!({ "text": "no topic" }));
}

/// A host keeping one text document per `(stream, key)`, as the hub's
/// `document` answers: the text after the request.
#[derive(Default)]
struct FakeDocs {
    texts: Mutex<std::collections::HashMap<(String, String), String>>,
    asked: Mutex<Vec<DocRequest>>,
}

#[async_trait]
impl LiveHost for FakeDocs {
    fn topics(&self, _stream: &str) -> Result<LiveTopics> {
        Ok(LiveTopics::Single)
    }

    fn publish(&self, _: &str, topic: Option<String>, _: Json) -> Result<Option<String>> {
        Ok(topic)
    }

    async fn document(&self, stream: &str, key: &str, request: DocRequest) -> Result<String> {
        if stream != "note_body" {
            return Err(Error::invalid(format!(
                "stream `{stream}` is not a document stream"
            )));
        }
        self.asked.lock().unwrap().push(request.clone());
        let mut texts = self.texts.lock().unwrap();
        let text = texts
            .entry((stream.to_owned(), key.to_owned()))
            .or_default();
        match request {
            DocRequest::Read => {}
            DocRequest::Append { text: more } => text.push_str(&more),
            DocRequest::Edit {
                index,
                delete,
                insert,
            } => {
                let mut units: Vec<u16> = text.encode_utf16().collect();
                let (i, d) = (index as usize, delete as usize);
                if i + d > units.len() {
                    return Err(Error::invalid("the edit is outside the text"));
                }
                units.splice(i..i + d, insert.encode_utf16());
                *text = String::from_utf16(&units).unwrap();
            }
        }
        Ok(text.clone())
    }
}

#[tokio::test]
async fn a_body_reads_and_edits_a_document() {
    let docs = Arc::new(FakeDocs::default());
    let host: Arc<dyn LiveHost> = docs.clone();
    let rt = CodeRuntime::with_workers(1);
    let out = rt
            .run(CodeCall {
                code: r#"const doc = live.document("note_body", row.id);
                         const empty = await doc.text();
                         await doc.append("Hello world");
                         await doc.insert(0, ">> ");
                         await doc.delete(0, 3);
                         const edited = await doc.edit(6, 5, "there");
                         let refused = null;
                         try { await live.document("job_status", 1).text(); }
                         catch (e) { refused = e.message; }
                         let typed = null;
                         try { doc.insert(-1, "x"); } catch (e) { typed = e.message; }
                         let unawaited = null;
                         try { JSON.stringify({ t: doc.text() }); } catch (e) { unawaited = e.message; }
                         return { empty, edited, refused, typed, unawaited };"#
                    .to_owned(),
                bindings: [("row".to_owned(), json!({ "id": 7 }))]
                    .into_iter()
                    .collect(),
                live: Some(&host),
                ..CodeCall::default()
            })
            .await
            .unwrap();
    assert_eq!(out["empty"], json!(""));
    assert_eq!(out["edited"], json!("Hello there"));
    assert!(
        out["refused"].as_str().unwrap().contains("not a document"),
        "{out}"
    );
    assert!(
        out["typed"].as_str().unwrap().contains("whole number"),
        "{out}"
    );
    assert!(
        out["unawaited"].as_str().unwrap().contains("not awaited"),
        "{out}"
    );
    let asked = docs.asked.lock().unwrap().clone();
    assert_eq!(
        asked[2],
        DocRequest::Edit {
            index: 0,
            delete: 0,
            insert: ">> ".into()
        }
    );
    assert_eq!(
        docs.texts.lock().unwrap()[&("note_body".to_owned(), "7".to_owned())],
        "Hello there",
        "a number key crosses as text"
    );
}

#[test]
fn a_document_request_is_read_from_its_plan() {
    let (stream, key, request) = document_request(&json!({
        "stream": "note_body", "key": 3, "op": "edit", "index": 1, "delete": 2, "insert": "x"
    }))
    .unwrap();
    assert_eq!((stream.as_str(), key.as_str()), ("note_body", "3"));
    assert_eq!(
        request,
        DocRequest::Edit {
            index: 1,
            delete: 2,
            insert: "x".into()
        }
    );
    assert!(document_request(&json!({ "stream": "s", "key": "1", "op": "explode" })).is_err());
    assert!(document_request(&json!({ "stream": "s" })).is_err());
    assert!(
        document_request(&json!({ "stream": "s", "key": "1", "op": "edit", "index": -1 })).is_err()
    );
}

/// A trace sink (a render cache's) gets every table the run's database
/// calls named, and whether it wrote — a read-only run is one an answer
/// may stand in for, and a run that wrote is not.
#[tokio::test]
async fn a_trace_records_the_tables_a_run_named_and_whether_it_wrote() {
    let db = FakeHost::rows(json!([{ "id": 1 }]));
    let rt = CodeRuntime::with_workers(1);
    let trace = trace_sink();
    let mut c = with_host(
        "await db.posts.rows(); await db.authors.where({ id: 1 }).rows(); return 1;",
        &*db,
    );
    c.trace = Some(Arc::clone(&trace));
    rt.run(c).await.unwrap();
    let read = read_trace(&trace);
    assert_eq!(
        read.tables.into_iter().collect::<Vec<_>>(),
        ["authors", "posts"]
    );
    assert!(!read.side_effects);

    let trace = trace_sink();
    let mut c = with_host(
        "await db.posts.rows(); await db.views.insert({ page: 1 }); return 1;",
        &*db,
    );
    c.trace = Some(Arc::clone(&trace));
    rt.run(c).await.unwrap();
    let wrote = read_trace(&trace);
    assert!(wrote.tables.contains("views"));
    assert!(wrote.side_effects, "an insert is a side effect");
}

#[tokio::test]
async fn a_stream_handle_is_a_reply_carrying_the_tables_the_run_read() {
    // TODO F7: `return live.stream(...)` answers like `response(...)`,
    // and the host adds the tables the run's database calls named.
    let live = Arc::new(FakeLive::default());
    let live_host: Arc<dyn LiveHost> = live.clone();
    let db = FakeHost::rows(json!([{ "id": 1, "mode": "edit" }]));
    let rt = CodeRuntime::with_workers(1);
    let out = rt
        .run(CodeCall {
            code: r#"const share = await db.share_links.rows();
                         await db.pads.rows();
                         await db.share_links.rows();
                         return live.stream("pad_doc", 42, {
                           write: share[0].mode === "edit", watch: ["audit"],
                         });"#
                .to_owned(),
            host: Some(&*db),
            live: Some(&live_host),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    let reply = crate::take_code_response(out).expect("a wrapped reply");
    assert_eq!(
        reply.stream,
        Some(crate::StreamReply {
            name: "pad_doc".to_owned(),
            key: "42".to_owned(),
            write: true,
            watch: vec!["audit".to_owned()],
            read: vec!["share_links".to_owned(), "pads".to_owned()],
        })
    );
    assert!(
        live.published.lock().unwrap().is_empty(),
        "nothing is published"
    );

    // A handle cannot be forged from JSON, and the options are checked.
    let forged = rt
        .run(CodeCall {
            code: r#"return { stream: { name: "pad_doc", key: "1", write: true } };"#.to_owned(),
            live: Some(&live_host),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    assert!(crate::take_code_response(forged).is_err(), "plain JSON");
    let refused = rt
        .run(CodeCall {
            code: r#"const why = [];
                         for (const f of [() => live.stream("", 1),
                                          () => live.stream("pad_doc"),
                                          () => live.stream("pad_doc", 1, { mode: "edit" }),
                                          () => live.stream("pad_doc", 1, { watch: "shares" })]) {
                           try { f(); } catch (e) { why.push(e.message); }
                         }
                         return why;"#
                .to_owned(),
            live: Some(&live_host),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    let why = refused.as_array().unwrap();
    assert_eq!(why.len(), 4, "{refused}");
    assert!(
        why[2].as_str().unwrap().contains("not an option"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_body_without_a_live_host_cannot_name_live() {
    let rt = CodeRuntime::with_workers(1);
    let error = rt
        .run(CodeCall {
            code: r#"live.publish("job_status", null, {});"#.to_owned(),
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("live is not defined"), "{error}");
}

/// A call that can reach the module functions and nothing else.
fn with_module_fns<'a>(code: &str, module_fns: &'a dyn ModuleFnHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        module_fns: Some(module_fns),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_calls_a_module_function_by_its_short_name_and_by_its_module() {
    let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]).answering(|plan| {
        Ok(json!(format!(
            "<p>{}</p>",
            plan["args"][0].as_str().unwrap()
        )))
    });
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"const short = await modfn.md_to_html("hi");
                   const exact = await modfn("@saltcorn/markdown").md_to_html("there");
                   return { short: short, exact: exact,
                            supplies: modfn.functions.map(f => f.module + ":" + f.name) };"#,
            &*mods,
        ))
        .await
        .unwrap();
    assert_eq!(out["short"], json!("<p>hi</p>"));
    assert_eq!(out["exact"], json!("<p>there</p>"));
    assert_eq!(out["supplies"], json!(["@saltcorn/markdown:md_to_html"]));
    // Both spellings send the same plan: a module, a function, positional
    // arguments, and the clock the op filled in.
    let asked = mods.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0]["module"], json!("@saltcorn/markdown"));
    assert_eq!(asked[0]["function"], json!("md_to_html"));
    assert_eq!(asked[0]["args"], json!(["hi"]));
    assert!(asked[0]["timeout_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn a_module_function_is_in_scope_by_its_bare_name_as_in_saltcorn_1() {
    // A v1 body: `await geocode_lat(q)` with no `modfn.` in front of it.
    let mods = FakeModuleFns::new(&[
        ("@saltcorn/nominatim-geocode", "geocode_lat"),
        ("@acme/odd", "row"),
        ("@acme/odd", "delete"),
        ("@acme/odd", "own"),
    ])
    .answering(|plan| Ok(json!({ "from": plan["function"], "args": plan["args"] })));
    let rt = CodeRuntime::new();
    let call = CodeCall {
        bindings: BTreeMap::from([("row".to_owned(), json!({ "postcode": "E1 7QX" }))]),
        ..with_module_fns(
            r#"const own = () => "the body's own";
                   return {
                     lat: await geocode_lat({ q: row.postcode }),
                     row: row.postcode,
                     own: own(),
                     reserved: await modfn.delete(),
                   };"#,
            &*mods,
        )
    };
    let out = rt.run(call).await.unwrap();
    assert_eq!(
        out["lat"],
        json!({ "from": "geocode_lat", "args": [{ "q": "E1 7QX" }] })
    );
    // A binding outranks a module function of the same name, and so does
    // the body's own declaration; a reserved word is reachable through
    // `modfn` and does not stop the body compiling.
    assert_eq!(out["row"], json!("E1 7QX"));
    assert_eq!(out["own"], json!("the body's own"));
    assert_eq!(out["reserved"]["from"], json!("delete"));
}

#[tokio::test]
async fn a_bare_name_two_modules_supply_names_both_modules() {
    let mods = FakeModuleFns::new(&[
        ("@saltcorn/nominatim-geocode", "geocode_lat"),
        ("@saltcorn/other-geocode", "geocode_lat"),
    ]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"try { await geocode_lat("here"); } catch (e) { return e.message; }"#,
            &*mods,
        ))
        .await
        .unwrap();
    let msg = out.as_str().unwrap();
    assert!(
        msg.contains("@saltcorn/other-geocode") && msg.contains("say which module"),
        "{msg}"
    );
}

#[tokio::test]
async fn two_modules_supplying_one_name_neither_shadows_the_other() {
    let mods = FakeModuleFns::new(&[
        ("@saltcorn/nominatim-geocode", "geocode_lat"),
        ("@saltcorn/other-geocode", "geocode_lat"),
    ])
    .answering(|plan| Ok(plan["module"].clone()));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"let ambiguous = null;
                   try { await modfn.geocode_lat("here"); }
                   catch (e) { ambiguous = e.message; }
                   return {
                     ambiguous: ambiguous,
                     first: await modfn("@saltcorn/nominatim-geocode").geocode_lat("here"),
                     second: await modfn("@saltcorn/other-geocode").geocode_lat("here"),
                   };"#,
            &*mods,
        ))
        .await
        .unwrap();
    // The short form does not pick one — it names both and the spelling
    // that works, because choosing by load order is a wrong answer.
    let ambiguous = out["ambiguous"].as_str().unwrap();
    assert!(
        ambiguous.contains("@saltcorn/nominatim-geocode"),
        "{ambiguous}"
    );
    assert!(ambiguous.contains("@saltcorn/other-geocode"), "{ambiguous}");
    assert!(ambiguous.contains("say which module"), "{ambiguous}");
    assert_eq!(out["first"], json!("@saltcorn/nominatim-geocode"));
    assert_eq!(out["second"], json!("@saltcorn/other-geocode"));
}

#[tokio::test]
async fn a_module_function_that_closes_over_its_configuration_sees_the_configured_value() {
    // The host stands in for the module's own state: what is asserted here
    // is that one configured value answers every call, which is what makes
    // a module a singleton and this seam a hop.
    let mods = FakeModuleFns::new(&[("@saltcorn/large-language-model", "llm_generate")]).answering(
        |plan| {
            Ok(json!(format!(
                "gpt-9 says: {}",
                plan["args"][0].as_str().unwrap_or("")
            )))
        },
    );
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"return await modfn.llm_generate("hello");"#,
            &*mods,
        ))
        .await
        .unwrap();
    assert_eq!(out, json!("gpt-9 says: hello"));
}

#[tokio::test]
async fn a_name_no_module_supplies_names_the_ones_that_exist() {
    let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
    let rt = CodeRuntime::new();
    let error = rt
        .run(with_module_fns(
            r#"return await modfn.md_to_htmll("x");"#,
            &*mods,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("md_to_htmll"), "{error}");
    assert!(error.contains("md_to_html"), "{error}");
    // And a module nobody installed says the same kind of thing.
    let error = rt
        .run(with_module_fns(
            r#"return await modfn("@saltcorn/nope").md_to_html("x");"#,
            &*mods,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("@saltcorn/nope"), "{error}");
    assert!(error.contains("@saltcorn/markdown"), "{error}");
}

#[tokio::test]
async fn an_argument_that_is_not_json_fails_naming_the_function() {
    let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"const results = [];
                   try { await modfn.md_to_html((x) => x); }
                   catch (e) { results.push(e.message); }
                   const cycle = {}; cycle.self = cycle;
                   try { await modfn.md_to_html(cycle); }
                   catch (e) { results.push(e.message); }
                   return results;"#,
            &*mods,
        ))
        .await
        .unwrap();
    // A function argument is dropped *silently* by JSON.stringify, which is
    // the mangled value §4a refuses; a cycle throws inside it.
    assert!(out[0].as_str().unwrap().contains("md_to_html"), "{out}");
    assert!(out[0].as_str().unwrap().contains("function"), "{out}");
    assert!(out[1].as_str().unwrap().contains("md_to_html"), "{out}");
    assert!(out[1].as_str().unwrap().contains("not JSON"), "{out}");
    // Neither reached the host.
    assert!(mods.asked().is_empty(), "{:?}", mods.asked());
}

#[tokio::test]
async fn a_module_function_that_failed_is_an_error_the_body_can_catch() {
    let mods = FakeModuleFns::new(&[("@saltcorn/nominatim-geocode", "geocode_lat")])
        .answering(|_| Err(Error::invalid("nominatim: connect ECONNREFUSED")));
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"try {
                     await modfn.geocode_lat("here");
                     return "no throw";
                   } catch (e) { return "caught: " + e.message; }"#,
            &*mods,
        ))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!("caught: invalid: nominatim: connect ECONNREFUSED")
    );
}

#[tokio::test]
async fn a_forgotten_await_on_a_module_function_says_so() {
    let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
    let rt = CodeRuntime::new();
    let out = rt
        .run(with_module_fns(
            r#"const pending = modfn.md_to_html("hi");
                   try { return JSON.stringify({ html: pending }); }
                   catch (e) { return e.message; }"#,
            &*mods,
        ))
        .await
        .unwrap();
    // Not `[object Promise]` in a column: the same treatment `db`'s promise
    // gets, in this surface's own words.
    assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
    assert!(out.as_str().unwrap().contains("modfn"), "{out}");
}

#[tokio::test]
async fn the_module_call_budget_stops_a_call_per_row() {
    let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            code: r#"for (let i = 0; i < 10; i++) await modfn.md_to_html("row " + i);
                         return "done";"#
                .to_owned(),
            module_fns: Some(&*mods),
            max_module_calls: 3,
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("more than 3 module functions"), "{error}");
    assert_eq!(mods.asked().len(), 3, "{:?}", mods.asked());
}

#[tokio::test]
async fn a_body_without_the_module_host_cannot_name_modfn() {
    let rt = CodeRuntime::new();
    let error = rt
        .run(CodeCall {
            code: r#"return await modfn.md_to_html("x");"#.to_owned(),
            ..CodeCall::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("modfn is not defined"), "{error}");
}

// -----------------------------------------------------------------------
// The v1 `Table` API, in the isolate that will serve it
// -----------------------------------------------------------------------

/// A snapshot in the shape `sc_api::code_host::schema` builds: two tables
/// with a key between them, an ownership formula that v1 can also read as a
/// field, and one of this server's own tables for `Table.find()` to leave
/// out.
fn library_snapshot() -> SchemaSnapshot {
    let field = |name: &str, typename: &str, sql: &str, pk: bool| {
        json!({
            "name": name, "label": name, "type": { "name": typename, "sql_name": sql },
            "typename": typename, "required": pk, "is_unique": pk, "primary_key": pk,
            "calculated": false, "stored": false, "expression": null,
            "is_fkey": false, "reftable_name": null, "refname": null, "reftype": null,
            "attributes": {}, "fieldview": null, "sublabel": null,
            "table_id": "books", "sql_name": name, "sql_type": sql,
        })
    };
    let author_key = json!({
        "name": "author", "label": "Author", "type": "Key to authors",
        "typename": "Key to authors", "required": false, "is_unique": false,
        "primary_key": false, "calculated": false, "stored": false, "expression": null,
        "is_fkey": true, "reftable_name": "authors", "refname": "id",
        "reftype": "Integer", "attributes": { "summary_field": "name" },
        "fieldview": "select", "sublabel": "who wrote it",
        "table_id": "books", "sql_name": "author", "sql_type": "int8",
    });
    let json = json!({ "tables": [
        {
            "id": "books", "name": "books", "label": "Books",
            "description": "the library", "primary_key": ["id"],
            "min_role_read": 40, "min_role_write": 20,
            "ownership_formula": "owner === user.id", "ownership_field_id": "owner",
            "provider_name": null, "provider_module": null, "is_system": false,
            "fields": [
                field("id", "Integer", "int8", true),
                field("title", "String", "text", false),
                field("pages", "Integer", "int8", false),
                author_key,
                field("owner", "Integer", "int8", false),
            ],
        },
        {
            "id": "authors", "name": "authors", "label": "Authors",
            "description": null, "primary_key": ["id"],
            "min_role_read": 100, "min_role_write": 20,
            "ownership_formula": null, "ownership_field_id": null,
            "provider_name": null, "provider_module": null, "is_system": false,
            "fields": [
                json!({
                    "name": "id", "label": "id", "type": { "name": "Integer", "sql_name": "int8" },
                    "typename": "Integer", "required": true, "is_unique": true,
                    "primary_key": true, "calculated": false, "stored": false,
                    "expression": null, "is_fkey": false, "reftable_name": null,
                    "refname": null, "reftype": null, "attributes": {},
                    "fieldview": null, "sublabel": null, "table_id": "authors",
                    "sql_name": "id", "sql_type": "int8",
                }),
                json!({
                    "name": "name", "label": "Name", "type": { "name": "String", "sql_name": "text" },
                    "typename": "String", "required": false, "is_unique": false,
                    "primary_key": false, "calculated": false, "stored": false,
                    "expression": null, "is_fkey": false, "reftable_name": null,
                    "refname": null, "reftype": null, "attributes": {},
                    "fieldview": null, "sublabel": null, "table_id": "authors",
                    "sql_name": "name", "sql_type": "text",
                }),
            ],
        },
        {
            "id": "_fd_modules", "name": "_fd_modules", "label": "_fd_modules",
            "description": null, "primary_key": ["id"],
            "min_role_read": 1, "min_role_write": 1,
            "ownership_formula": null, "ownership_field_id": null,
            "provider_name": null, "provider_module": null, "is_system": true,
            "fields": [],
        },
    ]});
    SchemaSnapshot::new(7, json.to_string())
}

/// A body run with the library snapshot on it, answering whatever it
/// returns.
async fn with_schema(rt: &CodeRuntime, snapshot: &SchemaSnapshot, code: &str) -> Json {
    let mut c = call(code);
    c.schema = Some(snapshot);
    rt.run(c).await.expect("the body ran")
}

/// The api, over the run's own snapshot — what Phase 6 binds as a run
/// parameter and what these tests build by hand until it does.
const MAKE: &str = "const { Table, Field } = __scMakeV1Api(null, __scSchema(7));";

#[tokio::test]
async fn v1s_where_vocabulary_becomes_this_servers() {
    // §5's table, line by line: every spelling v1 has for a condition, and
    // the one this server's filter object says it in. A translator that got
    // one of these wrong would not fail — it would compute the wrong answer
    // inside somebody's trigger.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
            &rt,
            &library_snapshot(),
            r#"const W = __scV1Where;
               return {
                 scalar:    W({ author: "Tolstoy" }),
                 null:      W({ author: null }),
                 gt:        W({ pages: { gt: 500 } }),
                 gt_equal:  W({ pages: { gt: 500, equal: true } }),
                 lt_equal:  W({ pages: { lt: 500, equal: true } }),
                 between:   W({ pages: { gt: 100, lt: 500 } }),
                 in:        W({ id: { in: [1, 2] } }),
                 not_in:    W({ id: { not: { in: [1, 2] } } }),
                 ilike:     W({ author: { ilike: "tol" } }),
                 fts:       W({ _fts: { searchTerm: "tol", fields: [
                              { name: "title", type: { sql_name: "text" } },
                              { name: "pages", type: { sql_name: "int8" } },
                              { name: "note", sql_type: "text" },
                              { name: "shout", type: { sql_name: "text" }, calculated: true },
                              { name: "kept", type: { sql_name: "text" }, calculated: true, stored: true },
                            ] } }),
                 fts_one:   W({ _fts: { searchTerm: "tol", fields: [{ name: "title", sql_type: "text" }] }, pages: 1 }),
                 fts_empty: W({ _fts: { searchTerm: "", fields: [{ name: "title", sql_type: "text" }] } }),
                 fts_none:  W({ _fts: { searchTerm: "tol", fields: [{ name: "pages", sql_type: "int8" }] } }),
                 full:      W({ author: { ilike: "tol", fullMatch: true } }),
                 or:        W({ or: [{ pages: 1 }, { pages: 2 }] }),
                 and:       W({ and: [{ pages: 1 }, { title: "x" } ] }),
                 not:       W({ not: { pages: 1 } }),
                 field_or:  W({ pages: { or: [{ gt: 1 }, { lt: 0 }] } }),
                 array_and: W({ pages: [{ gt: 1 }, { lt: 9 }] }),
                 false:     W({ _false: true }),
                 two:       W({ author: "T", pages: { gt: 1 } }),
                 empty:     W({}),
                 undef:     W({ author: undefined }),
                 none:      W(undefined),
               };"#,
        )
        .await;

    assert_eq!(out["scalar"], json!({ "author": { "eq": "Tolstoy" } }));
    assert_eq!(out["null"], json!({ "author": { "is_null": true } }));
    assert_eq!(out["gt"], json!({ "pages": { "gt": 500 } }));
    assert_eq!(out["gt_equal"], json!({ "pages": { "gte": 500 } }));
    assert_eq!(out["lt_equal"], json!({ "pages": { "lte": 500 } }));
    assert_eq!(
        out["between"],
        json!({ "and": [{ "pages": { "gt": 100 } }, { "pages": { "lt": 500 } }] })
    );
    assert_eq!(out["in"], json!({ "id": { "in": [1, 2] } }));
    // v1's negated membership is one operator here rather than a negated
    // one, which is the statement anybody reading the SQL expects.
    assert_eq!(out["not_in"], json!({ "id": { "nin": [1, 2] } }));
    // v1's implicit `%…%`, and the spelling that turns it off.
    assert_eq!(out["ilike"], json!({ "author": { "ilike": "%tol%" } }));
    assert_eq!(out["full"], json!({ "author": { "ilike": "tol" } }));
    // v1's `_fts`: any text column containing the term, stored calculated
    // columns included and live ones not, as v1's `ftsFieldsSqlExpr` picks.
    assert_eq!(
        out["fts"],
        json!({ "or": [
                { "title": { "ilike": "%tol%" } },
                { "note": { "ilike": "%tol%" } },
                { "kept": { "ilike": "%tol%" } },
            ] })
    );
    assert_eq!(
        out["fts_one"],
        json!({ "and": [{ "title": { "ilike": "%tol%" } }, { "pages": { "eq": 1 } }] })
    );
    // An empty search is no condition, as v1's `LIKE '%%'` is none.
    assert_eq!(out["fts_empty"], json!(null));
    // No text column: v1 searches `''`, which nothing contains.
    assert_eq!(out["fts_none"], json!({ "formula": "false" }));
    assert_eq!(
        out["or"],
        json!({ "or": [{ "pages": { "eq": 1 } }, { "pages": { "eq": 2 } }] })
    );
    assert_eq!(
        out["and"],
        json!({ "and": [{ "pages": { "eq": 1 } }, { "title": { "eq": "x" } }] })
    );
    assert_eq!(out["not"], json!({ "not": { "pages": { "eq": 1 } } }));
    // A field-level `or` is an `or` of two conditions **on that field**.
    assert_eq!(
        out["field_or"],
        json!({ "or": [{ "pages": { "gt": 1 } }, { "pages": { "lt": 0 } }] })
    );
    assert_eq!(
        out["array_and"],
        json!({ "and": [{ "pages": { "gt": 1 } }, { "pages": { "lt": 9 } }] })
    );
    // There is no `false` in the filter vocabulary, so v1's "match nothing"
    // is said in the other spelling this seam carries.
    assert_eq!(out["false"], json!({ "formula": "false" }));
    assert_eq!(
        out["two"],
        json!({ "and": [{ "author": { "eq": "T" } }, { "pages": { "gt": 1 } }] })
    );
    // Nothing said is nothing sent — never a predicate that matches none.
    assert_eq!(out["empty"], Json::Null);
    assert_eq!(out["undef"], Json::Null, "v1 drops an undefined value");
    assert_eq!(out["none"], Json::Null);
}

#[tokio::test]
async fn every_v1_where_this_server_cannot_say_is_refused_by_name() {
    // Each of these is a SQL construct the plan seam deliberately does not
    // carry or a feature this server does not have. Dropping one on the
    // floor would compute the wrong answer quietly, so each says the key it
    // refused.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        r#"const W = __scV1Where;
               const cases = {
                 inSelect: () => W({ id: { inSelect: { table: "x", field: "y" } } }),
                 inSelectWithLevels: () => W({ id: { inSelectWithLevels: {} } }),
                 json: () => W({ meta: { json: ["a", 1] } }),
                 slugify: () => W({ slugify: "x" }),
                 day_only: () => W({ when: { day_only: true } }),
                 eq: () => W({ eq: [{ field: "a" }, { field: "b" }] }),
                 RegExp: () => W({ author: /tol/ }),
                 Symbol: () => W({ author: Symbol("raw sql") }),
                 starts_with: () => W({ author: { starts_with: "tol" } }),
               };
               const out = {};
               for (const key of Object.keys(cases)) {
                 try { cases[key](); out[key] = "did not throw"; }
                 catch (e) { out[key] = e.message; }
               }
               return out;"#,
    )
    .await;

    for (key, wanted) in [
        ("inSelect", "inSelect"),
        ("inSelectWithLevels", "inSelectWithLevels"),
        ("json", "json"),
        ("slugify", "slugify"),
        ("day_only", "day_only"),
        ("eq", "eq"),
        ("RegExp", "regular expression"),
        ("Symbol", "Symbol"),
        ("starts_with", "starts_with"),
    ] {
        let message = out[key].as_str().unwrap_or_default();
        assert!(
            message.contains(wanted),
            "the refusal of {key} does not name it: {message}"
        );
    }
}

#[tokio::test]
async fn v1s_selopts_lower_and_an_unknown_one_is_refused() {
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        r#"const S = __scV1Selopts;
               const refused = (f) => { try { f(); return "did not throw"; }
                                        catch (e) { return e.message; } };
               return {
                 plain:   S({ orderBy: "title", limit: 10, offset: 5 }),
                 desc:    S({ orderBy: "title", orderDesc: true }),
                 object:  S({ orderBy: { field: "title", desc: true } }),
                 fields:  S({ fields: ["title", "pages"] }),
                 forUser: S({ forUser: { id: 7, role_id: 40 } }),
                 byId:    S({ forUser: 7 }),
                 public:  S({ forPublic: true }),
                 nothing: S(undefined),
                 unknown: refused(() => S({ cached: true })),
                 both:    refused(() => S({ forUser: { id: 1 }, forPublic: true })),
                 near:    refused(() => S({ orderBy: { operator: "near" } })),
               };"#,
    )
    .await;

    assert_eq!(
        out["plain"],
        json!({ "order": [{ "field": "title", "dir": "asc" }], "limit": 10, "offset": 5 })
    );
    assert_eq!(
        out["desc"],
        json!({ "order": [{ "field": "title", "dir": "desc" }] })
    );
    assert_eq!(
        out["object"],
        json!({ "order": [{ "field": "title", "dir": "desc" }] })
    );
    assert_eq!(out["fields"], json!({ "select": ["title", "pages"] }));
    // v1 says whose view of the data this is with a user; the plan says it
    // with an authority, and the named user goes through the same ownership
    // functions `asUser()` goes through.
    assert_eq!(out["forUser"], json!({ "authority": { "user": 7 } }));
    assert_eq!(out["byId"], json!({ "authority": { "user": 7 } }));
    assert_eq!(out["public"], json!({ "authority": "public" }));
    assert_eq!(out["nothing"], json!({}));
    let unknown = out["unknown"].as_str().unwrap_or_default();
    assert!(
        unknown.contains("cached") && unknown.contains("orderBy"),
        "{unknown}"
    );
    assert!(
        out["both"]
            .as_str()
            .unwrap_or_default()
            .contains("forPublic"),
        "{out}"
    );
    assert!(
        out["near"].as_str().unwrap_or_default().contains("near"),
        "{out}"
    );
}

/// v1's relation lists, which a Saltcorn UI pattern's configuration steps
/// read (TODO "Saltcorn UI" Phase 10): answered from the snapshot, as the
/// rest of a table's metadata is, rather than refused.
#[tokio::test]
async fn the_v1_table_answers_its_relations_from_the_snapshot() {
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const authors = Table.findOne("authors");
                   const parents = await books.get_parent_relations();
                   const children = await authors.get_child_relations();
                   const joined = await books.get_child_relations(true);
                   return {{
                     parent_fields: parents.parent_field_list,
                     parent_keys: parents.parent_relations.map((r) => r.key_field.name + ">" + r.table.name),
                     child_fields: children.child_field_list,
                     child_tables: children.child_relations.map((r) => r.table.name),
                     own_children: (await books.get_child_relations()).child_field_list,
                     joined_fields: joined.child_field_list,
                     joined_through: joined.child_relations.map((r) => r.through.name),
                     authors_parents: (await authors.get_parent_relations(true, true)).parent_field_list,
                   }};"#
            ),
        )
        .await;
    let parent_fields = out["parent_fields"].as_array().unwrap();
    assert!(parent_fields.contains(&json!("author.name")), "{out}");
    assert!(
        parent_fields
            .iter()
            .all(|f| f.as_str().unwrap().starts_with("author.")),
        "{out}"
    );
    assert_eq!(out["parent_keys"], json!(["author>authors"]));
    assert_eq!(out["child_fields"], json!(["books.author"]));
    assert_eq!(out["child_tables"], json!(["books"]));
    assert_eq!(out["own_children"], json!([]));
    // Through its key to `authors`, the books of the same author.
    assert_eq!(out["joined_fields"], json!(["author->books.author"]));
    assert_eq!(out["joined_through"], json!(["author"]));
    assert_eq!(out["authors_parents"], json!([]));
}

#[tokio::test]
async fn the_v1_table_answers_its_metadata_synchronously() {
    // The definition of done's first four lines: none of this awaits
    // anything, none of it costs a host call, and this run has no host at
    // all — which is the point of the snapshot.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{MAKE}
                   const books = Table.findOne("books");
                   const author = books.getField("author");
                   return {{
                     name: books.name,
                     label: books.label,
                     pk_name: books.pk_name,
                     pk_type: books.pk_type,
                     composite: books.composite_pk_names,
                     sql_name: books.sql_name,
                     min_role_read: books.min_role_read,
                     fields: books.fields.map((f) => f.name),
                     getFields: (await books.getFields()).length,
                     is_fkey: author.is_fkey,
                     reftable_name: author.reftable_name,
                     reftype: author.reftype,
                     pretty_type: author.pretty_type,
                     fieldview: author.fieldview,
                     sublabel: author.sublabel,
                     summary: author.attributes.summary_field,
                     type_name: books.getField("title").type_name,
                     sql_type: books.getField("title").sql_type,
                     id_is_name: books.getField("title").id,
                     path: books.getField("author.name").table_id,
                     own_table: author.table.name,
                     foreign: books.getForeignKeys().map((f) => f.name),
                     owner: books.owner_fieldname(),
                     formula: books.ownership_formula,
                     santized_name: books.santized_name,
                     owner_row: books.is_owner({{ id: 7 }}, {{ id: 1, owner: 7 }}),
                     other_row: books.is_owner({{ id: 7 }}, {{ id: 2, owner: 8 }}),
                     nobody: books.is_owner(undefined, {{ id: 1, owner: 7 }}),
                     no_rule: Table.findOne("authors").is_owner({{ id: 7 }}, {{ id: 7 }}),
                     cached: Table.findOne("books") === books,
                     by_object: Table.findOne({{ name: "authors" }}).name,
                     missing: Table.findOne("nope") === undefined,
                     tables: Table.find().map((t) => t.name),
                     json_name: books.to_json().name,
                     json_fields: books.to_json().fields.length,
                     one_field: Field.findOne({{ table_id: "books", name: "title" }}).label,
                     keys: Field.find({{ is_fkey: true }}).map((f) => f.name),
                     cached_fields: Field.findCached({{ name: "id" }}).length,
                     label_to_name: Field.labelToName("First Name!"),
                     name_to_label: Field.nameToLabel("first_name"),
                   }};"#
        ),
    )
    .await;

    assert_eq!(out["name"], json!("books"));
    assert_eq!(out["label"], json!("Books"));
    assert_eq!(out["pk_name"], json!("id"));
    assert_eq!(out["pk_type"], json!("Integer"));
    // v1's getter: `null` for a one-column key, which v1's patterns test for
    // truth (`edit.ts` takes its composite-key path on anything truthy).
    assert_eq!(out["composite"], json!(null));
    assert_eq!(out["sql_name"], json!("\"books\""));
    assert_eq!(out["min_role_read"], json!(40));
    assert_eq!(
        out["fields"],
        json!(["id", "title", "pages", "author", "owner"])
    );
    assert_eq!(out["getFields"], json!(5));
    // v1's own property names, with v1's values behind them.
    assert_eq!(out["is_fkey"], json!(true));
    assert_eq!(out["reftable_name"], json!("authors"));
    assert_eq!(out["reftype"], json!("Integer"));
    assert_eq!(out["pretty_type"], json!("Key to authors"));
    assert_eq!(out["fieldview"], json!("select"));
    assert_eq!(out["sublabel"], json!("who wrote it"));
    assert_eq!(out["summary"], json!("name"));
    assert_eq!(out["type_name"], json!("String"));
    assert_eq!(out["sql_type"], json!("text"));
    // A field's id is its name, because this server identifies it by name.
    assert_eq!(out["id_is_name"], json!("title"));
    // A dotted path is the *other* table's field.
    assert_eq!(out["path"], json!("authors"));
    assert_eq!(out["own_table"], json!("books"));
    assert_eq!(out["foreign"], json!(["author"]));
    assert_eq!(out["owner"], json!("owner"));
    assert_eq!(out["formula"], json!("owner === user.id"));
    // v1's `sqlsanitize(name)`, which names a table's search state.
    assert_eq!(out["santized_name"], json!("books"));
    // v1's `is_owner`: the formula over the row and the user, and false for
    // nobody and for a table with no rule.
    assert_eq!(out["owner_row"], json!(true));
    assert_eq!(out["other_row"], json!(false));
    assert_eq!(out["nobody"], json!(false));
    assert_eq!(out["no_rule"], json!(false));
    // v1's is a state cache and plugins compare what comes out of it.
    assert_eq!(out["cached"], json!(true));
    assert_eq!(out["by_object"], json!("authors"));
    assert_eq!(out["missing"], json!(true));
    // This server's own tables are not the application's, and a plugin
    // listing tables meant the application's.
    assert_eq!(out["tables"], json!(["books", "authors"]));
    assert_eq!(out["json_name"], json!("books"));
    assert_eq!(out["json_fields"], json!(5));
    assert_eq!(out["one_field"], json!("title"));
    assert_eq!(out["keys"], json!(["author"]));
    assert_eq!(out["cached_fields"], json!(2), "both tables have an id");
    assert_eq!(out["label_to_name"], json!("first_name"));
    // v1's `nameToLabel` is `name.split("_").join(" ")`: no capital.
    assert_eq!(out["name_to_label"], json!("first name"));
}

/// The api over a sender that **records** what it is handed and answers
/// what the method expects back — which is all a plan-shape test needs, and
/// is what the live tests in `sc-api` put a real host behind.
const SPY: &str = r#"const sent = [];
       const answers = { select: [], aggregate: {}, grouped: [] };
       const { Table, Field } = __scMakeV1Api((plan) => {
         sent.push(plan);
         if (plan.render) return Promise.resolve({ sql: "select …", values: [1] });
         if (plan.op === "aggregate") {
           return Promise.resolve(plan.group ? answers.grouped : answers.aggregate);
         }
         return Promise.resolve(answers.select);
       }, __scSchema(7));"#;

#[tokio::test]
async fn every_v1_read_is_one_plan_of_the_seam_db_already_speaks() {
    // §1: a `getRows` is a `Plan { op: Select, … }`, so it goes through the
    // catalog's name resolution, the shared operator vocabulary, §7.3's
    // ownership rule, the row cap and the call budget — because it *is* the
    // plan a `db.books.rows()` sends. What is asserted here is exactly that
    // equality; that the plan then answers is the live tests' business.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{SPY}
                   const books = Table.findOne("books");
                   answers.select = [{{ id: 1, title: "Orlando" }}];
                   answers.aggregate = {{ count: 3 }};
                   answers.grouped = [{{ author: 1, _fd_n: 2 }}, {{ author: 2, _fd_n: 1 }}];
                   const rows = await books.getRows({{ pages: {{ gt: 300 }} }},
                                                    {{ orderBy: "title", limit: 10 }});
                   const one = await books.getRow({{ id: 1 }});
                   const none = await (async () => {{
                     answers.select = [];
                     const r = await books.getRow({{ id: 99 }});
                     answers.select = [{{ id: 1, title: "Orlando" }}];
                     return r;
                   }})();
                   const n = await books.countRows({{ author: 1 }});
                   const values = await books.distinctValues("author");
                   const fromField = await books.getField("author").distinct_values();
                   return {{
                     rows: rows, one: one, none: none, n: n, values: values,
                     fromField: fromField, sent: sent,
                   }};"#
        ),
    )
    .await;

    assert_eq!(out["rows"], json!([{ "id": 1, "title": "Orlando" }]));
    assert_eq!(out["one"], json!({ "id": 1, "title": "Orlando" }));
    // v1's `getRow` answers null when there is no row, and never undefined:
    // a plugin tests it with `if (row)` and JSON carries the one and not the
    // other.
    assert_eq!(out["none"], Json::Null);
    assert_eq!(out["n"], json!(3));
    // A distinct read answers v1's plain array of values, not the groups it
    // is computed as.
    assert_eq!(out["values"], json!([1, 2]));
    assert_eq!(
        out["fromField"],
        json!([1, 2]),
        "the same read from the field"
    );

    let sent = out["sent"].as_array().expect("the plans");
    assert_eq!(
        sent[0],
        json!({
            "op": "select", "table": "books",
            "where": { "pages": { "gt": 300 } },
            "order": [{ "field": "title", "dir": "asc" }],
            "limit": 10,
        })
    );
    assert_eq!(
        sent[1],
        json!({
            "op": "select", "table": "books",
            "where": { "id": { "eq": 1 } }, "limit": 1,
        }),
        "`getRow` is `getRows` with the bound this method's name promises"
    );
    assert_eq!(
        sent[3],
        json!({
            "op": "aggregate", "table": "books",
            "where": { "author": { "eq": 1 } },
            "aggregate": [{ "alias": "count", "fn": "count", "arg": Json::Null }],
        }),
        "a count is the database's, so the row cap never enters into it"
    );
    assert_eq!(
        sent[4],
        json!({
            "op": "aggregate", "table": "books", "group": ["author"],
            "aggregate": [{ "alias": "_fd_n", "fn": "count", "arg": Json::Null }],
            "order": [{ "field": "author", "dir": "asc" }],
        }),
        "distinct values are groups"
    );
}

#[tokio::test]
async fn an_aggregation_query_lowers_to_the_plans_own_aggregate() {
    // §3.3: v1's aggregation spec, answering one object ungrouped and an
    // array grouped — which is v1's convention and this server's, because a
    // grouped answer has no one value to be.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{SPY}
                   const books = Table.findOne("books");
                   answers.aggregate = {{ n: 3, longest: 412 }};
                   answers.grouped = [{{ author: 1, n: 2 }}];
                   const flat = await books.aggregationQuery({{
                     n: {{ aggregate: "count" }},
                     longest: {{ field: "pages", aggregate: "max" }},
                   }});
                   const grouped = await books.aggregationQuery(
                     {{ n: {{ aggregate: "count" }} }},
                     {{ where: {{ pages: {{ gt: 100 }} }}, groupBy: "author" }}
                   );
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     flat: flat, grouped: grouped, sent: sent,
                     latest: said(() => books.aggregationQuery(
                       {{ x: {{ field: "pages", aggregate: "Latest published" }} }})),
                     child: said(() => books.aggregationQuery(
                       {{ x: {{ table: "reviews", ref: "book", aggregate: "count" }} }})),
                     unknown: said(() => books.aggregationQuery({{ n: {{ aggregate: "count" }} }},
                                                               {{ cached: true }})),
                   }};"#
        ),
    )
    .await;

    assert_eq!(out["flat"], json!({ "n": 3, "longest": 412 }));
    assert_eq!(out["grouped"], json!([{ "author": 1, "n": 2 }]));
    let sent = out["sent"].as_array().expect("the plans");
    assert_eq!(
        sent[0],
        json!({
            "op": "aggregate", "table": "books",
            "aggregate": [
                { "alias": "n", "fn": "count", "arg": Json::Null },
                { "alias": "longest", "fn": "max", "arg": "pages" },
            ],
        })
    );
    assert_eq!(
        sent[1],
        json!({
            "op": "aggregate", "table": "books",
            "aggregate": [{ "alias": "n", "fn": "count", "arg": Json::Null }],
            "where": { "pages": { "gt": 100 } },
            "group": ["author"],
        })
    );
    // The two aggregate shapes that are a *subquery* over child rows are
    // refused here rather than approximated: this question is about the
    // table's own rows, and `getJoinedRows` is the one with children.
    assert!(
        out["latest"]
            .as_str()
            .unwrap_or_default()
            .contains("Latest published"),
        "{out}"
    );
    assert!(
        out["child"]
            .as_str()
            .unwrap_or_default()
            .contains("getJoinedRows"),
        "{out}"
    );
    assert!(
        out["unknown"]
            .as_str()
            .unwrap_or_default()
            .contains("cached"),
        "{out}"
    );
}

#[tokio::test]
async fn a_joined_read_lowers_to_paths_and_relations_in_one_plan() {
    // §6, which is the piece of v1 whose vocabulary looks least like this
    // server's and turns out to fit best: `joinFields` is a Ⱶ-path
    // projection and `aggregations` is an inverse-relation one, so a joined
    // read is **one** select plan and needs no host operation of its own.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{SPY}
                   const books = Table.findOne("books");
                   answers.select = [{{ id: 1, title: "Orlando", writer: "Woolf", stars: 4.5 }}];
                   const rows = await books.getJoinedRows({{
                     where: {{ pages: {{ gt: 100 }} }},
                     joinFields: {{ writer: {{ ref: "author", target: "name" }} }},
                     aggregations: {{
                       stars: {{ table: "reviews", ref: "book", field: "stars",
                                 aggregate: "avg" }},
                       reviews: {{ table: "reviews", ref: "book", aggregate: "count" }},
                       critics: {{ table: "reviews", ref: "book", field: "critic",
                                   aggregate: "count distinct" }},
                       latest: {{ table: "reviews", ref: "book", field: "stars",
                                  aggregate: "Latest posted" }},
                     }},
                     orderBy: "title", limit: 5,
                   }});
                   const one = await books.getJoinedRow({{ joinFields: {{}} }});
                   const query = await books.getJoinedQuery({{
                     joinFields: {{ writer: {{ ref: "author", target: "name" }} }},
                   }});
                   return {{ rows: rows, one: one, query: query, sent: sent }};"#
        ),
    )
    .await;

    let sent = out["sent"].as_array().expect("the plans");
    assert_eq!(
        sent[0],
        json!({
            "op": "select", "table": "books",
            "where": { "pages": { "gt": 100 } },
            "select": [
                "id", "title", "pages", "author", "owner",
                { "alias": "writer", "formula": "authorⱵname" },
                { "alias": "stars", "formula": "reviewsↃbook.avg(\"stars\")" },
                { "alias": "reviews", "formula": "reviewsↃbook.length" },
                { "alias": "critics", "formula": "reviewsↃbook.distinct(\"critic\").length" },
                { "alias": "latest", "formula": "reviewsↃbook.maxBy(\"posted\").stars" },
            ],
            "order": [{ "field": "title", "dir": "asc" }],
            "limit": 5,
        }),
        "the row is the table's own fields, and the join and the aggregations \
             are projections beside them"
    );
    assert_eq!(sent[1]["limit"], json!(1), "`getJoinedRow` bounds itself");
    assert_eq!(
        sent[2]["render"],
        json!(true),
        "`getJoinedQuery` asks for the statement rather than the rows"
    );
    assert_eq!(
        out["one"],
        json!({ "id": 1, "title": "Orlando", "writer": "Woolf", "stars": 4.5 })
    );
    // v1's own shape, and this server will not run it back for you.
    assert_eq!(out["query"], json!({ "sql": "select …", "values": [1] }));
}

#[tokio::test]
async fn every_join_option_this_server_cannot_lower_is_refused_by_name() {
    // §6's list, each naming itself. A join field this server dropped on the
    // floor would answer a row with a column missing, which a v1 list view
    // renders as an empty cell rather than as a failure.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{SPY}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   const join = (spec) => said(() => books.getJoinedRows({{ joinFields: {{ x: spec }} }}));
                   const agg = (spec) => said(() => books.getJoinedRows({{ aggregations: {{ x: spec }} }}));
                   return {{
                     through: join({{ ref: "author", target: "name", through: "publisher" }}),
                     ontable: join({{ ref: "author", target: "name", ontable: "authors" }}),
                     rename: join({{ ref: "author", target: "name", rename_object: ["a"] }}),
                     lookup: join({{ ref: "author", target: "name", lookupFunction: "f" }}),
                     unknown_join: join({{ ref: "author", target: "name", summary: "name" }}),
                     not_a_name: join({{ ref: "author", target: "name; drop" }}),
                     valueFormula: agg({{ table: "reviews", ref: "book", aggregate: "count",
                                          valueFormula: "x + 1" }}),
                     agg_where: agg({{ table: "reviews", ref: "book", aggregate: "count",
                                       where: {{ stars: 5 }} }}),
                     array_agg: agg({{ table: "reviews", ref: "book", field: "stars",
                                       aggregate: "array_agg" }}),
                     no_field: agg({{ table: "reviews", ref: "book", aggregate: "avg" }}),
                     option: said(() => books.getJoinedRows({{ starts_with: {{ title: "O" }} }})),
                   }};"#
            ),
        )
        .await;

    for (key, wanted) in [
        ("through", "through"),
        ("ontable", "ontable"),
        ("rename", "rename_object"),
        ("lookup", "lookupFunction"),
        ("unknown_join", "summary"),
        ("not_a_name", "target"),
        ("valueFormula", "valueFormula"),
        ("agg_where", "where"),
        ("array_agg", "array_agg"),
        ("no_field", "field"),
        ("option", "starts_with"),
    ] {
        let message = out[key].as_str().unwrap_or_default();
        assert!(
            message.contains(wanted),
            "the refusal of {key} does not name it: {message}"
        );
    }
}

/// The write spy: the same arrangement the read spy is, answering the
/// shapes the host answers a write with — an insert its written row, an
/// update and a delete their `{ updated | deleted, ids }` — and a third
/// sender for the one method that runs a trigger rather than touching a row.
const WRITE_SPY: &str = r#"const sent = [];
       const answers = {
         insert: { id: 7, title: "Orlando" },
         select: [{ id: 1, title: "Orlando", read: false }],
         write: { updated: 1, ids: [1] },
         trigger: { ran: true },
         fail: null,
       };
       const { Table, Field } = __scMakeV1Api((plan) => {
         sent.push(plan);
         if (answers.fail) return Promise.reject(new Error(answers.fail));
         if (plan.op === "insert") return Promise.resolve(answers.insert);
         if (plan.op === "select") return Promise.resolve(answers.select);
         return Promise.resolve(answers.write);
       }, __scSchema(7), (request) => {
         sent.push(request);
         if (answers.fail) return Promise.reject(new Error(answers.fail));
         return Promise.resolve(answers.trigger);
       });"#;

#[tokio::test]
async fn every_v1_write_is_one_plan_of_the_seam_db_already_speaks() {
    // §4: a v1 write is the plan a `db.books.insert(…)` sends, so it goes
    // through the row layer — coerced, validated, and **an event a trigger
    // sees**. What is asserted here is that equality and v1's own answers;
    // that the plan then writes is the live tests' business.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const id = await books.insertRow({{ title: "Orlando" }});
                   const tried = await books.tryInsertRow({{ title: "Orlando" }});
                   const updated = await books.updateRow({{ pages: 10 }}, 1);
                   const triedUp = await books.tryUpdateRow({{ pages: 10 }}, 1);
                   const deleted = await books.deleteRows({{ pages: {{ lt: 1 }} }});
                   const toggled = await books.toggleBool(1, "owner");
                   return {{ id: id, tried: tried, updated: updated,
                             triedUp: triedUp, deleted: deleted, toggled: toggled,
                             sent: sent }};"#
        ),
    )
    .await;

    // v1's `insertRow` answers the primary key of the row it wrote.
    assert_eq!(out["id"], json!(7));
    assert_eq!(out["tried"], json!({ "success": 7 }));
    // v1's `updateRow` answers nothing at all when it worked, which is what
    // `if (err)` in eight years of plugins is written against.
    assert_eq!(out["updated"], Json::Null);
    assert_eq!(out["triedUp"], json!({ "success": true }));
    assert_eq!(out["deleted"], Json::Null);
    assert_eq!(out["toggled"], Json::Null);

    let sent = out["sent"].as_array().expect("the plans");
    assert_eq!(
        sent[0],
        json!({ "op": "insert", "table": "books", "values": { "title": "Orlando" } })
    );
    assert_eq!(
        sent[2],
        json!({
            "op": "update", "table": "books",
            "values": { "pages": 10 }, "where": { "id": { "eq": 1 } },
        }),
        "v1 addresses a row by its key, and this seam by the `where` that names it"
    );
    assert_eq!(
        sent[4],
        json!({ "op": "delete", "table": "books", "where": { "pages": { "lt": 1 } } })
    );
    // A toggle is two round trips, because an assignment on this seam is a
    // value and not an expression over the row.
    assert_eq!(
        sent[5],
        json!({
            "op": "select", "table": "books",
            "where": { "id": { "eq": 1 } }, "limit": 1,
        })
    );
    assert_eq!(
        sent[6],
        json!({
            "op": "update", "table": "books",
            "values": { "owner": true }, "where": { "id": { "eq": 1 } },
        }),
        "the row said false, so the write says true"
    );
}

#[tokio::test]
async fn v1s_user_argument_is_the_named_user_authority() {
    // §4: v1 says whose write this is with an argument, and it lowers to the
    // very same `{ user: id }` `forUser` does — the named user, through the
    // same ownership functions `asUser()` goes through. It can only narrow:
    // the body already runs as admin.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const user = {{ id: "0d4e1e1e-0000-4000-8000-000000000001", role: 40 }};
                   await books.insertRow({{ title: "x" }}, user);
                   await books.updateRow({{ pages: 1 }}, 1, user.id);
                   await books.deleteRows({{ id: 1 }}, user);
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     sent: sent,
                     no_id: said(() => books.insertRow({{ title: "x" }}, {{ email: "a@b" }})),
                     nonsense: said(() => books.insertRow({{ title: "x" }}, true)),
                   }};"#
        ),
    )
    .await;

    let sent = out["sent"].as_array().expect("the plans");
    let named = json!({ "user": "0d4e1e1e-0000-4000-8000-000000000001" });
    for (i, op) in [(0, "insert"), (1, "update"), (2, "delete")] {
        assert_eq!(sent[i]["op"], json!(op));
        assert_eq!(sent[i]["authority"], named, "{op} runs as the named user");
    }
    assert!(
        out["no_id"]
            .as_str()
            .unwrap_or_default()
            .contains("has no "),
        "a user with no id is refused rather than dropped: {out}"
    );
    assert!(
        out["nonsense"]
            .as_str()
            .unwrap_or_default()
            .contains("user row or a user id"),
        "{out}"
    );
}

#[tokio::test]
async fn a_failed_v1_write_answers_in_v1s_two_shapes() {
    // v1's return convention, preserved: `updateRow` answers a **string**
    // and `tryUpdateRow` an `{ error }`, because `const err = await
    // t.updateRow(…); if (err)` is how a v1 plugin reads a refusal. An
    // `insertRow` throws, which is what v1's own `tryInsertRow` catches.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   answers.fail = "not authorized";
                   const update = await books.updateRow({{ pages: 1 }}, 1);
                   const tried = await books.tryUpdateRow({{ pages: 1 }}, 1);
                   const triedIn = await books.tryInsertRow({{ title: "x" }});
                   let threw = "did not throw";
                   try {{ await books.insertRow({{ title: "x" }}); }}
                   catch (e) {{ threw = e.message; }}
                   answers.fail = null;
                   answers.write = {{ updated: 0, ids: [] }};
                   const nothing = await books.updateRow({{ pages: 1 }}, 99);
                   return {{ update: update, tried: tried, triedIn: triedIn,
                             threw: threw, nothing: nothing }};"#
        ),
    )
    .await;
    assert_eq!(out["update"], json!("not authorized"));
    assert_eq!(out["tried"], json!({ "error": "not authorized" }));
    assert_eq!(out["triedIn"], json!({ "error": "not authorized" }));
    assert_eq!(out["threw"], json!("not authorized"));
    // And a write that touched **nothing** is a refusal too, not a success:
    // a delegated update resolves its rows through the delegated read, so a
    // row this user may not see is a row that matched nothing — the same
    // not-found an absent row gets, deliberately.
    assert!(
        out["nothing"]
            .as_str()
            .unwrap_or_default()
            .contains("may not write it"),
        "{out}"
    );
}

#[tokio::test]
async fn a_v1_run_trigger_is_the_dispatchers_own_run() {
    // §4.5: through *the* dispatcher, so `only_if`, the role floor and the
    // cascade bound all still apply — and as a named user when v1 named one.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const ran = await books.run_trigger("archive", {{ id: 1 }});
                   await books.run_trigger({{ name: "archive" }}, {{ id: 2 }},
                                           "0d4e1e1e-0000-4000-8000-000000000001");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   const {{ Table: T2 }} = __scMakeV1Api(null, __scSchema(7));
                   return {{
                     ran: ran, sent: sent,
                     unnamed: said(() => books.run_trigger(7)),
                     hostless: said(() => T2.findOne("books").run_trigger("archive")),
                   }};"#
        ),
    )
    .await;
    assert_eq!(out["ran"], json!({ "ran": true }));
    let sent = out["sent"].as_array().expect("the requests");
    assert_eq!(
        sent[0],
        json!({ "trigger": "archive", "payload": { "id": 1 } })
    );
    assert_eq!(
        sent[1],
        json!({
            "trigger": "archive", "payload": { "id": 2 },
            "authority": { "user": "0d4e1e1e-0000-4000-8000-000000000001" },
        })
    );
    assert!(
        out["unnamed"]
            .as_str()
            .unwrap_or_default()
            .contains("by name"),
        "{out}"
    );
    assert!(
        out["hostless"]
            .as_str()
            .unwrap_or_default()
            .contains("no trigger host"),
        "a run with no trigger sender says so rather than doing nothing: {out}"
    );
}

#[tokio::test]
async fn every_v1_write_option_this_server_cannot_honour_is_refused_by_name() {
    // The `selopts` rule, on the writing side: v1's `noTrigger` is the one
    // that matters, because a write here **is** an event and a plugin that
    // passed it did so to stop a cascade this server bounds another way.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     noTrigger: said(() => books.updateRow({{ pages: 1 }}, 1, null, true)),
                     option: said(() =>
                       books.updateRow({{ pages: 1 }}, 1, null, {{ noTrigger: true }})),
                     collector: said(() =>
                       books.updateRow({{ pages: 1 }}, 1, null, {{ resultCollector: {{}} }})),
                     no_id: said(() => books.updateRow({{ pages: 1 }})),
                     array: said(() => books.insertRow([{{ title: "x" }}])),
                     no_field: said(() => books.toggleBool(1, "nope")),
                   }};"#
        ),
    )
    .await;
    for (key, wanted) in [
        ("noTrigger", "noTrigger"),
        ("option", "noTrigger"),
        ("collector", "resultCollector"),
        ("no_id", "primary key"),
        ("array", "one row"),
        ("no_field", "nope"),
    ] {
        let message = out[key].as_str().unwrap_or_default();
        assert!(
            message.contains(wanted),
            "the refusal of {key} does not name it: {message}"
        );
    }
}

#[tokio::test]
async fn a_read_with_no_database_says_so_and_the_metadata_still_answers() {
    // The division the snapshot buys, at its edge: a run with the schema and
    // no host answers every property and refuses every read. Phase 5's
    // `onLoad` is exactly this, and a `getRows` that answered `[]` there
    // would have a plugin conclude the table is empty.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        r#"const { Table } = __scMakeV1Api(null, __scSchema(7));
               const books = Table.findOne("books");
               const said = (f) => { try { f(); return "did not throw"; }
                                     catch (e) { return e.message; } };
               return {
                 pk: books.pk_name,
                 rows: said(() => books.getRows({})),
                 count: said(() => books.countRows()),
                 field: said(() => books.getField("author").distinct_values()),
               };"#,
    )
    .await;
    assert_eq!(out["pk"], json!("id"));
    for key in ["rows", "count", "field"] {
        let message = out[key].as_str().unwrap_or_default();
        assert!(message.contains("no database host"), "{key}: {message}");
    }
    assert!(
        out["rows"].as_str().unwrap_or_default().contains("getRows"),
        "the refusal names the method the plugin called: {out}"
    );
}

#[tokio::test]
async fn a_v1_field_cannot_be_assigned_to() {
    // v1 code assigns to a field and expects it to matter — that is what
    // `Field.update` is for. Here it would change a copy of a snapshot, so
    // it is refused at the property rather than swallowed: a body that is
    // not in strict mode would otherwise never learn.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{MAKE}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "accepted"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     field: said(() => {{ books.getField("title").label = "Nope"; }}),
                     added: said(() => {{ books.getField("title").invented = 1; }}),
                     table: said(() => {{ books.name = "nope"; }}),
                   }};"#
        ),
    )
    .await;
    for key in ["field", "added", "table"] {
        let message = out[key].as_str().unwrap_or_default();
        assert!(message.contains("would change a copy"), "{key}: {message}");
    }
}

#[tokio::test]
async fn every_refused_v1_method_names_itself() {
    // §9's whole tier boundary, walked from the one list it is generated
    // from: reachable as a property, fatal on call, naming the path. A
    // method implemented later is deleted from that list by the edit that
    // implements it — and `installRefusals` refuses to shadow a method that
    // is there, so the list and the implementation cannot disagree.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{MAKE}
                   const books = Table.findOne("books");
                   const owners = {{
                     Table: Table, table: books,
                     Field: Field, field: books.getField("title"),
                   }};
                   const out = {{}};
                   for (const path of __scV1Refused()) {{
                     const dot = path.indexOf(".");
                     const owner = owners[path.slice(0, dot)];
                     const name = path.slice(dot + 1);
                     // `View.`, `state.` …: the view runtime's models, which a
                     // code body has none of (sc-module's tests walk those).
                     if (!owner) continue;
                     if (typeof owner[name] !== "function") {{
                       out[path] = "not reachable as a property";
                       continue;
                     }}
                     try {{ owner[name](); out[path] = "did not throw"; }}
                     catch (e) {{ out[path] = e.message; }}
                   }}
                   out.implemented = __scV1Refused().indexOf("table.getRows") < 0;
                   return out;"#
        ),
    )
    .await;

    let refusals = out.as_object().expect("an object of refusals");
    assert!(refusals.len() > 30, "the refusal tier is the list: {out}");
    assert_eq!(
        out["implemented"],
        json!(true),
        "an implemented method is not refused"
    );
    for (path, message) in refusals {
        if path == "implemented" {
            continue;
        }
        let message = message.as_str().unwrap_or_default();
        assert!(
            message.contains(path) && message.contains("is not available"),
            "{path} does not name itself: {message}"
        );
    }
    // And the three kinds of reason are each said, rather than one blanket
    // sentence for everything.
    let schema = out["Table.create"].as_str().unwrap_or_default();
    assert!(schema.contains("introspects the schema"), "{schema}");
    assert!(
        out["table.compress_history"]
            .as_str()
            .unwrap_or_default()
            .contains("nothing to compress"),
        "{out}"
    );
    assert!(
        out["table.field_options"]
            .as_str()
            .unwrap_or_default()
            .contains("view builder"),
        "{out}"
    );
    // The builder's join and relation pickers are answered (TODO "The
    // builder" 5.2), so they left the list in the edit that implemented them.
    for method in [
        "table.get_join_field_options",
        "table.get_relation_options",
        "table.get_relation_data",
    ] {
        assert!(
            refusals.get(method).is_none(),
            "{method} is implemented and still on the refusal list"
        );
    }
    // And the writes are gone from it, because the edit that implemented
    // them deleted their lines — which is the only way `installRefusals`
    // would have let the api build at all.
    for method in [
        "table.insertRow",
        "table.updateRow",
        "table.deleteRows",
        "table.toggleBool",
        "table.run_trigger",
        // Row history (TODO-history H6.1).
        "table.get_history",
        "table.restore_row_version",
        "table.undo_row_changes",
        "table.redo_row_changes",
    ] {
        assert!(
            refusals.get(method).is_none(),
            "{method} is implemented and still on the refusal list"
        );
    }
}

#[tokio::test]
async fn the_tutorial_compatibility_table_names_every_v1_method() {
    // Phase 7.3's table, checked against the thing it describes. A
    // compatibility table is a promise about a surface, and a surface that
    // grows a method is a promise that quietly stops being true — so the
    // document is read here and matched against the api itself: every
    // refusal on §9's one list is named in the table, every method the api
    // really implements is named in the table, and the table names nothing
    // that is neither.
    let rt = CodeRuntime::with_workers(1);
    let out = with_schema(
        &rt,
        &library_snapshot(),
        &format!(
            r#"{MAKE}
                   const books = Table.findOne("books");
                   const owners = {{
                     Table: Table, table: books,
                     Field: Field, field: books.getField("title"),
                   }};
                   // Table and Field only: the list's `View.`, `state.` …
                   // lines are the view runtime's, which no code body has and
                   // this tutorial does not describe.
                   const refused = __scV1Refused().filter(
                     (path) => owners[path.slice(0, path.indexOf("."))] !== undefined
                   );
                   const implemented = [];
                   for (const prefix of Object.keys(owners)) {{
                     const owner = owners[prefix];
                     for (const name of Object.getOwnPropertyNames(owner)) {{
                       const path = prefix + "." + name;
                       if (refused.indexOf(path) >= 0) continue;
                       if (typeof owner[name] !== "function") continue;
                       implemented.push(path);
                     }}
                   }}
                   return {{ refused: refused, implemented: implemented }};"#
        ),
    )
    .await;

    // And the sentence the tutorial prints for the line an admin will
    // paste is the sentence this crate throws (`REQUIRE`), because a
    // document that quotes an error is a document that can quote a stale
    // one.
    let doc = tutorial_triggers();
    for fragment in [
        "is not available in a code body",
        "delete the line that requires them",
    ] {
        assert!(
            doc.contains(fragment),
            "docs/tutorial-triggers.md no longer quotes `{fragment}`"
        );
    }

    let listed = compatibility_table();
    let named = |key: &str| -> Vec<String> {
        out[key]
            .as_array()
            .expect("an array of method paths")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect()
    };
    for path in named("refused") {
        assert!(
            listed.contains(&path),
            "`{path}` is refused and is not in the compatibility table in \
                 docs/tutorial-triggers.md"
        );
    }
    for path in named("implemented") {
        assert!(
            listed.contains(&path),
            "`{path}` is implemented and is not in the compatibility table in \
                 docs/tutorial-triggers.md"
        );
    }
    let real: Vec<String> = named("refused")
        .into_iter()
        .chain(named("implemented"))
        .collect();
    for path in &listed {
        assert!(
            real.contains(path),
            "the compatibility table in docs/tutorial-triggers.md names `{path}`, \
                 which is neither implemented nor refused"
        );
    }
}

/// The triggers tutorial, read from the repository this crate is in.
fn tutorial_triggers() -> String {
    let mut dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    while !dir.join("docs/tutorial-triggers.md").is_file() {
        assert!(dir.pop(), "no docs/tutorial-triggers.md above this crate");
    }
    std::fs::read_to_string(dir.join("docs/tutorial-triggers.md"))
        .expect("read docs/tutorial-triggers.md")
}

/// The method paths the tutorial's compatibility table lists: every
/// backticked `Table.`/`table.`/`Field.`/`field.` name in its first column,
/// which is one row per member and several members to a row where they
/// refuse for the same reason.
fn compatibility_table() -> Vec<String> {
    let doc = tutorial_triggers();
    let start = doc
        .find("### The Saltcorn 1 `Table` and `Field` compatibility table")
        .expect("the compatibility table's heading");
    let rest = &doc[start..];
    // The table ends where the prose about the properties begins.
    let end = rest
        .find("\nThe properties come with")
        .unwrap_or(rest.len());
    let mut out = Vec::new();
    for row in rest[..end].lines().filter(|l| l.starts_with('|')) {
        let cell = row.split('|').nth(1).unwrap_or_default();
        for part in cell.split('`').skip(1).step_by(2) {
            let path = part.trim();
            let owner = path.split('.').next().unwrap_or_default();
            if matches!(owner, "Table" | "table" | "Field" | "field")
                && path.split('.').count() == 2
            {
                out.push(path.to_string());
            }
        }
    }
    assert!(
        out.len() > 40,
        "the compatibility table did not parse: {out:?}"
    );
    out
}

#[tokio::test]
async fn a_run_with_no_snapshot_gets_a_table_that_says_why() {
    // Phase 5's `onLoad` and any other context with no schema. A
    // `Table.findOne` that answered `undefined` for every table would have
    // a plugin compute the wrong answer instead of failing.
    let rt = CodeRuntime::with_workers(1);
    let out = rt
        .run(call(
            r#"const { Table, Field } = __scMakeV1Api(null, null);
                   const said = (f) => { try { return f(); } catch (e) { return e.message; } };
                   return {
                     table: said(() => Table.findOne("books")),
                     find: said(() => Field.find({})),
                     create: said(() => Table.create("books")),
                     pure: Field.nameToLabel("first_name"),
                   };"#,
        ))
        .await
        .expect("the body ran");
    assert!(
        out["table"]
            .as_str()
            .unwrap_or_default()
            .contains("no schema snapshot"),
        "{out}"
    );
    assert!(
        out["find"]
            .as_str()
            .unwrap_or_default()
            .contains("no schema snapshot"),
        "{out}"
    );
    // A method that is refused everywhere is refused here too, in its own
    // words rather than the snapshot's.
    assert!(
        out["create"]
            .as_str()
            .unwrap_or_default()
            .contains("Table.create"),
        "{out}"
    );
    assert_eq!(out["pure"], json!("first name"));
}

// -----------------------------------------------------------------------
// Phase 6: the classes as run parameters
// -----------------------------------------------------------------------

#[tokio::test]
async fn a_body_with_a_host_is_handed_v1s_table_and_field() {
    // §8: `Table` and `Field` are minted per run beside `db`, so a body
    // writes v1's own first line without building anything. The metadata
    // half costs no host call and the read is one plan on this run's own
    // sender — which is what `plans()` here proves.
    let host = FakeHost::rows(json!([{ "id": 3, "title": "Dune" }]));
    let mut c = with_host(
        r#"const books = Table.findOne({ name: "books" });
               const rows = await books.getRows({ pages: { gt: 100 } },
                                                { orderBy: "title", limit: 10 });
               return {
                 pk: books.pk_name,
                 fkey: books.getField("author").is_fkey,
                 label: Field.nameToLabel("first_name"),
                 titles: rows.map((b) => b.title),
               };"#,
        &*host,
    );
    let snapshot = library_snapshot();
    c.schema = Some(&snapshot);
    let out = CodeRuntime::with_workers(1).run(c).await.expect("it ran");
    assert_eq!(out["pk"], json!("id"), "no host call answered this");
    assert_eq!(out["fkey"], json!(true));
    // v1's `nameToLabel` does not capitalise.
    assert_eq!(out["label"], json!("first name"));
    assert_eq!(out["titles"], json!(["Dune"]));
    // One plan, and it is the read — the metadata went nowhere near the
    // host.
    let plans = host.plans();
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(plans[0]["op"], json!("select"), "{}", plans[0]);
    assert_eq!(plans[0]["table"], json!("books"), "{}", plans[0]);
}

#[tokio::test]
async fn without_a_host_the_classes_are_not_in_scope_at_all() {
    // The rule `db` has (§8): no host is no parameter, so naming one is the
    // ReferenceError it has always been rather than a class that fails on
    // use — and it is that even when the run carries a schema, because the
    // metadata half of `Table` is not the half that matters here.
    let rt = CodeRuntime::with_workers(1);
    let snapshot = library_snapshot();
    // Named rather than `typeof`, which answers "undefined" for a name
    // that was never declared: what is asserted here is that reaching for
    // the class fails, and that is what a body would do.
    let mut c = call("return Table.findOne(\"books\");");
    c.schema = Some(&snapshot);
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("Table is not defined"), "{err}");
    let mut c = call("return Field.nameToLabel(\"x\");");
    c.schema = Some(&snapshot);
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("Field is not defined"), "{err}");
}

#[tokio::test]
async fn v1s_own_first_line_says_what_to_write_instead() {
    // §8 predicted a `SyntaxError` here, and it is wrong about the
    // mechanism: the wrapper compiles a body as a **nested** function, so
    // `const Table = require(…)` shadows the parameter rather than
    // redeclaring it and compiles perfectly well. What it then hits is
    // `require`, so that is where the sentence belongs — and without it an
    // admin who pasted six lines of Saltcorn 1 reads `require is not a
    // function` on line 1, which is a message about the wrong thing.
    let host = FakeHost::rows(json!([]));
    let mut c = with_host(
        r#"const Table = require("@saltcorn/data/models/table");
               return 1;"#,
        &*host,
    );
    let snapshot = library_snapshot();
    c.schema = Some(&snapshot);
    let err = CodeRuntime::with_workers(1)
        .run(c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("@saltcorn/data/models/table"), "{err}");
    assert!(err.contains("not available in a code body"), "{err}");
    assert!(err.contains("already in scope"), "{err}");
    // A body with no host has no classes to be told about, and is told the
    // half of it that is still true.
    let err = CodeRuntime::with_workers(1)
        .run(call(r#"return require("axios");"#))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`axios`"), "{err}");
    assert!(err.contains("not a module"), "{err}");
    assert!(!err.contains("already in scope"), "{err}");
}

#[tokio::test]
async fn a_binding_that_collides_with_the_v1_classes_is_refused() {
    // 6.2: the caller's own binding, refused where `db`'s is and in the same
    // words — because the alternative is a redeclaration deep inside the
    // generated wrapper, which is a message nobody could act on.
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::with_workers(1);
    for name in ["Table", "Field"] {
        let mut c = with_host("return 1;", &*host);
        c.bindings.insert(name.into(), json!(1));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains(&format!("`{name}` collides")), "{err}");
        assert!(err.contains("Saltcorn 1"), "{err}");
    }
    // With no host there are no classes, so the names are the caller's to
    // bind — a workflow step whose context has a variable called `Table`
    // does not stop working because this milestone happened.
    let mut c = call("return Table + Field;");
    c.bindings.insert("Table".into(), json!(1));
    c.bindings.insert("Field".into(), json!(2));
    assert_eq!(rt.run(c).await.unwrap(), json!(3));
}

// -----------------------------------------------------------------------
// Code libraries (TODO F3.2)
// -----------------------------------------------------------------------

fn libraries(set: &[(&str, &str)]) -> Option<Arc<CodeLibraries>> {
    Some(Arc::new(CodeLibraries::new(set.iter().copied())))
}

#[tokio::test]
async fn two_bodies_share_a_library_and_an_edit_reaches_both() {
    let rt = CodeRuntime::with_workers(1);
    let v1 = libraries(&[("greet", "exports.hello = (who) => `hello ${who}`;")]);
    let mut first = call(r#"return lib("greet").hello("a");"#);
    first.libraries = v1.clone();
    let mut second = call(r#"const { hello } = lib("greet"); return hello("b") + "!";"#);
    second.libraries = v1;
    assert_eq!(rt.run(first.clone()).await.unwrap(), json!("hello a"));
    assert_eq!(rt.run(second.clone()).await.unwrap(), json!("hello b!"));

    // An edit is new text and so a new key: the same isolate compiles it
    // and both bodies answer from it on their next run.
    let v2 = libraries(&[("greet", "module.exports = { hello: (who) => `hi ${who}` };")]);
    first.libraries = v2.clone();
    second.libraries = v2;
    assert_eq!(rt.run(first).await.unwrap(), json!("hi a"));
    assert_eq!(rt.run(second).await.unwrap(), json!("hi b!"));
}

#[tokio::test]
async fn a_library_is_evaluated_once_per_run_with_the_runs_own_handles() {
    let host = FakeHost::rows(json!([{ "id": 1 }, { "id": 2 }]));
    let rt = CodeRuntime::with_workers(1);
    let set = libraries(&[
        (
            "counter",
            "let n = 0; exports.next = () => ++n; exports.who = () => user && user.email;",
        ),
        (
            "books",
            "exports.count = async () => (await db.books.rows()).length;",
        ),
    ]);
    let mut c = with_host(
        r#"
            const a = lib("counter"), b = lib("counter");
            a.next(); b.next();
            return [a === b, a.next(), a.who(), await lib("books").count()];
            "#,
        &*host,
    );
    c.bindings
        .insert("user".into(), json!({ "email": "a@b.c" }));
    c.libraries = set.clone();
    assert_eq!(
        rt.run(c.clone()).await.unwrap(),
        json!([true, 3, "a@b.c", 2])
    );
    // The library's `db` call went over this run's host.
    assert_eq!(host.plans().len(), 1);
    // A second run starts from a fresh evaluation: module state never
    // carries from one caller to the next.
    assert_eq!(rt.run(c).await.unwrap(), json!([true, 3, "a@b.c", 2]));
}

#[tokio::test]
async fn a_body_rebinding_db_does_not_reach_a_library_and_a_library_may_rebind_its_own() {
    // As `code_api_js.md` says: a library gets the run's handles, not the
    // importer's variables, and its own top-level `db = db.asAdmin()` holds
    // for every function it exports.
    let host = FakeHost::rows(json!([]));
    let rt = CodeRuntime::with_workers(1);
    let mut c = with_host(
        r#"
            db = db.asAdmin();
            await db.books.rows();
            await lib("plain").read();
            await lib("admin").read();
            return null;
            "#,
        &*host,
    );
    c.libraries = libraries(&[
        ("plain", "exports.read = () => db.books.rows();"),
        (
            "admin",
            "db = db.asAdmin(); exports.read = () => db.books.rows();",
        ),
    ]);
    rt.run(c).await.unwrap();
    let authorities: Vec<Json> = host
        .plans()
        .iter()
        .map(|plan| plan["authority"].clone())
        .collect();
    assert_eq!(
        authorities,
        [json!("admin"), json!("default"), json!("admin")]
    );
}

#[tokio::test]
async fn a_library_has_no_handle_the_importing_body_lacks() {
    let rt = CodeRuntime::with_workers(1);
    let mut c = call(r#"return lib("net").get();"#);
    c.libraries = libraries(&[(
        "net",
        "exports.get = () => typeof db + ' ' + typeof process;",
    )]);
    // No host: `db` is no parameter of the library either, and the node
    // globals are shadowed there as they are in a body.
    assert_eq!(rt.run(c).await.unwrap(), json!("undefined undefined"));
}

#[tokio::test]
async fn a_cycle_names_the_chain() {
    let rt = CodeRuntime::with_workers(1);
    let mut c = call(r#"return lib("a");"#);
    c.libraries = libraries(&[
        ("a", r#"exports.b = lib("b");"#),
        ("b", r#"exports.c = lib("c");"#),
        ("c", r#"exports.a = lib("a");"#),
    ]);
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("cycle: a → b → c → a"), "{err}");
}

#[tokio::test]
async fn an_unknown_library_is_named_with_what_could_be_imported() {
    let rt = CodeRuntime::with_workers(1);
    let mut c = call(r#"return lib("nope");"#);
    c.libraries = libraries(&[("auth", ""), ("util", "")]);
    let err = rt.run(c).await.unwrap_err().to_string();
    assert!(err.contains("no code library named `nope`"), "{err}");
    assert!(err.contains("auth, util"), "{err}");
    // With no set at all, `lib` is still in scope and says so.
    let err = rt
        .run(call(r#"return lib("auth");"#))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("can import none"), "{err}");
    // And `require` now points at it.
    let err = rt
        .run(call(r#"return require("./auth");"#))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("lib(\"name\")"), "{err}");
}

#[tokio::test]
async fn a_library_that_does_not_compile_breaks_only_its_importers() {
    let rt = CodeRuntime::with_workers(1);
    let set = libraries(&[
        ("broken", "exports.a = 1;\nexports.b = = 2;"),
        ("ok", "exports.v = 7;"),
    ]);
    let mut fine = call(r#"return lib("ok").v;"#);
    fine.libraries = set.clone();
    assert_eq!(rt.run(fine).await.unwrap(), json!(7));
    let mut importer = call(r#"return lib("broken").a;"#);
    importer.libraries = set;
    let err = rt.run(importer).await.unwrap_err().to_string();
    assert!(err.contains("`broken` does not compile"), "{err}");
    assert!(err.contains("SyntaxError"), "{err}");
    assert!(err.contains("(line 2)"), "{err}");
}

#[tokio::test]
async fn the_save_check_names_the_line_and_runs_nothing() {
    let engine = crate::DenoEvaluator::new();
    let ran = crate::check_code_library(&engine, "exports.a = 1;\nthrow new Error('ran');")
        .await
        .unwrap();
    assert_eq!(ran, None, "compiling is not running");
    let err = crate::check_code_library(&engine, "exports.a = 1;\n\nexports.b = (;")
        .await
        .unwrap()
        .expect("a syntax error");
    assert!(err.starts_with("SyntaxError"), "{err}");
    assert!(err.contains("(line 3)"), "{err}");
}

#[tokio::test]
async fn a_library_may_declare_a_name_its_importer_has_as_a_handle() {
    // The save check's run has no `db`, so it has no `models` either; an
    // importer with a host has both. A library declaring either must work
    // in both, not compile in one and redeclare in the other.
    let code = "async function models() { return 'mine'; }\nconst db = 2;\n\
                    exports.v = async () => [await models(), db];";
    let engine = crate::DenoEvaluator::new();
    assert_eq!(
        crate::check_code_library(&engine, code).await.unwrap(),
        None
    );
    let host = FakeHost::rows(json!([]));
    let mut c = with_host(r#"return await lib("own").v();"#, &*host);
    c.libraries = libraries(&[("own", code)]);
    let rt = CodeRuntime::with_workers(1);
    assert_eq!(rt.run(c).await.unwrap(), json!(["mine", 2]));
}

/// `ratelimit` as a body sees it (TODO F4.5): run through a real isolate.
mod ratelimit_tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{CodeCall, CodeRuntime};

    fn call(code: &str, scope: &str) -> CodeCall<'static> {
        CodeCall {
            code: code.to_owned(),
            timeout: Some(Duration::from_secs(60)),
            ratelimit_scope: Some(scope.to_owned()),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn five_wrong_guesses_lock_one_key_and_only_that_key() {
        let rt = CodeRuntime::with_workers(1);
        let scope = format!("app:test-{}", std::process::id());
        let out = rt
            .run(call(
                r#"
                const opts = { max: 5, window: "15m" };
                const a = [];
                for (let i = 0; i < 6; i++) a.push(ratelimit.hit("share:a", opts));
                return { a, b: ratelimit.hit("share:b", opts) };
                "#,
                &scope,
            ))
            .await
            .unwrap();
        assert_eq!(out["a"], json!([true, true, true, true, true, false]));
        assert_eq!(out["b"], json!(true));

        // `check` throws `httpError(429)` with the wait in its body…
        let err = rt
            .run(call(
                r#"ratelimit.check("share:a", { max: 5, window: "15m" }); return "in";"#,
                &scope,
            ))
            .await
            .unwrap_err();
        let reply = err.http_reply().expect("an httpError");
        assert_eq!(reply.status, 429);
        let body: serde_json::Value = serde_json::from_str(reply.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["error"], json!("too many requests"));
        assert!(body["retry_after"].as_u64().unwrap() >= 1, "{body}");

        // …and another application's `share:a` is a count of its own.
        let other = rt
            .run(call(
                r#"ratelimit.check("share:a", { max: 5, window: "15m" }); return "in";"#,
                &format!("{scope}-other"),
            ))
            .await
            .unwrap();
        assert_eq!(other, json!("in"));
    }

    #[tokio::test]
    async fn bad_arguments_are_refused_by_name() {
        let rt = CodeRuntime::with_workers(1);
        for (code, expected) in [
            (r#"ratelimit.hit("", { max: 1 })"#, "takes a key first"),
            (r#"ratelimit.hit("k", { max: 0 })"#, "needs max"),
            (
                r#"ratelimit.hit("k", { max: 1, window: "a fortnight" })"#,
                "not a window",
            ),
        ] {
            let err = rt
                .run(call(code, "app:bad-args"))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{code}: {err}");
        }
        // A window in milliseconds works too.
        let ok = rt
            .run(call(
                r#"return ratelimit.hit("ms", { max: 1, window: 500 });"#,
                "app:ms",
            ))
            .await
            .unwrap();
        assert_eq!(ok, json!(true));
    }
}
