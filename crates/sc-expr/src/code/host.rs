//! The host traits a code body reaches through, and the guest-language seam.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::{Error, Result};
use serde_json::Value as Json;

use super::CodeCall;

/// The host surface a code body can reach: one JSON request in, one JSON value
/// out. **The** seam of §15 — a Python or Rust adapter implements the same trait
/// against the same plans, which is why this takes JSON rather than anything
/// shaped like a query.
///
/// `Err` is thrown into the guest as an ordinary `Error` at the call site, so a
/// body may catch it (a delegated write that is refused, say, and a fallback).
#[async_trait]
pub trait CodeHost: Send + Sync {
    /// Answer one plan. Awaited by the guest rather than blocking it: the
    /// isolate is free while this future is pending, so a slow answer costs a
    /// pending promise and not a thread.
    async fn call(&self, request: Json) -> Result<Json>;
}

/// The **second** host surface: one outbound HTTP request in, one response out.
///
/// Separate from [`CodeHost`] although the shape is the same, for two reasons
/// worth keeping apart. It is a different capability — a body may have tables
/// and no network, or the reverse — and it is implemented somewhere else: the
/// table host holds this server's catalog, while this holds an HTTP client, and
/// nothing sensible implements both. A run carries at most one of each.
///
/// The request and the response are plain JSON, exactly as a plan is, so §15's
/// other guest languages inherit `fetch` the way they inherit `db`:
///
/// ```json
/// { "url": "https://api.example.com/hooks", "method": "POST",
///   "headers": [["content-type", "application/json"]],
///   "body": "{\"id\":1}", "body_base64": false, "timeout_ms": 4000 }
/// ```
///
/// ```json
/// { "status": 200, "status_text": "OK", "url": "https://api.example.com/hooks",
///   "redirected": false, "headers": [["content-type", "application/json"]],
///   "text": "{\"ok\":true}" }
/// ```
///
/// `timeout_ms` is filled in by the op from what the guest asked for and what is
/// left of the run's wall clock, so an implementation may take it as given. An
/// `Err` is a **transport** failure and is thrown into the guest as a
/// `TypeError`, which is what the web API does; a response with a status the
/// server did not like is not an error at all — it comes back as an ordinary
/// response whose `ok` is false, again as the web API has it.
#[async_trait]
pub trait FetchHost: Send + Sync {
    /// Send one request and answer its response.
    async fn fetch(&self, request: Json) -> Result<Json>;
}

/// The **third** host surface: one file operation in, one JSON answer out.
///
/// Separate from the other two for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no files),
/// it is implemented somewhere else (over the catalog's connected file stores,
/// in `sc-api`), and it spends a budget of its own. And, as with them, what
/// crosses is plain JSON, so §15's other guest languages inherit `fs` the way
/// they inherit `db`:
///
/// ```json
/// { "op": "read", "store": "uploads", "path": "notes/a.txt",
///   "authority": "admin", "encoding": "text" }
/// ```
///
/// ```json
/// { "text": "hello" }
/// ```
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a refused write, a missing file — so a body may catch it. Nothing here is
/// trusted: the store name, the path, and the authority are all re-checked by
/// the implementation, which is the one that knows what stores exist and what
/// rule guards each path.
#[async_trait]
pub trait FileHost: Send + Sync {
    /// Answer one file operation.
    async fn files(&self, request: Json) -> Result<Json>;

    /// The store names to bind into the run's `fs.stores`, so `fs("typo")` can
    /// fail **at once** and name what does exist rather than deferring to the
    /// first operation.
    ///
    /// Synchronous, because the guest's `fs(name)` is: a body writes
    /// `fs("uploads").open(…)` in one expression and there is nothing to await
    /// in the middle of it. A host that cannot enumerate its stores without I/O
    /// answers with the default — an empty list, which the guest reads as "no
    /// list to check against" and lets every name through to be decided by the
    /// operation itself.
    fn store_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The **fourth** host surface: one trigger run in, one JSON result out.
///
/// Separate from the other three for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no way to
/// run anything else), it is implemented somewhere else (over the server's one
/// trigger dispatcher, in `sc-api`), and it spends a budget of its own. What
/// crosses is plain JSON, so §15's other guest languages inherit `trigger` the
/// way they inherit `db`:
///
/// ```json
/// { "trigger": "archive_done", "payload": { "before": "2026-08-01" },
///   "authority": "admin", "timeout_ms": 4750 }
/// ```
///
/// The answer is what the trigger's action returned, or `null` when its
/// `only_if` declined — the same value its caller gets through `POST
/// {mount}/actions/{name}` or the admin's Run button, because it is the same
/// call. `timeout_ms` is filled in by the op from what is left of the run's wall
/// clock, so an implementation may take it as given rather than deciding how
/// long a child may hold its parent.
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a trigger that failed, one that is disabled, one the caller may not run — so
/// a body may catch it and carry on. Nothing here is trusted: the name and the
/// authority are re-checked by the implementation, which is the one that knows
/// what triggers exist and what floor each has.
#[async_trait]
pub trait TriggerHost: Send + Sync {
    /// Run one trigger and answer what its action returned.
    async fn run(&self, request: Json) -> Result<Json>;

    /// The trigger names to bind into the run's `trigger.names`, so
    /// `trigger("typo")` can fail **at once** and name what does exist rather
    /// than deferring to the run itself.
    ///
    /// Synchronous, for [`FileHost::store_names`]' reason: the guest's
    /// `trigger(name)` is one expression with nothing to await in the middle of
    /// it. A host that cannot enumerate without I/O answers with the default —
    /// an empty list, which the guest reads as "no list to check against".
    fn trigger_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Live updates as a guest sees them: `live.publish(stream, topic, value)`
/// (TODO.md "Live updates" §7, L2.8), and what the `publish` action calls.
///
/// A sixth host, beside `db`, `fetch`, `fs`, `trigger` and `modfn`, and unlike
/// them **synchronous**: publishing is a non-blocking send onto a running
/// stream (§14.3's rule that nobody may block a flow), so there is nothing for
/// a body to await and nothing to time out. It is implemented by the server's
/// live hub (`sc-live`) and installed on the catalog, which is how a code body,
/// a Python body and an action all reach the same one without any of them
/// naming the crate that implements it.
///
/// What crosses is plain values, so §15's other guest languages inherit it the
/// way they inherit `db`. The implementation checks everything — that the
/// stream exists here, is running, is one Saltcorn may publish to, that the
/// topic is the kind it takes and that the value matches its element type —
/// and an `Err` is thrown into the guest as an ordinary error a body can catch.
///
/// Server-side publishing is **trusted** (§3 rule 8): a body is admin-authored
/// configuration, as an `insert_row` is, so it may publish to any topic. No
/// caller's role is consulted here.
///
/// **Documents** (L5.5) are the one asynchronous part: `live.document(stream,
/// key)` reads and edits a collaborative document, which may have to be loaded
/// from and persisted to its row. [`document`](LiveHost::document) is
/// defaulted to a refusal, so a host without documents need not mention it.
#[async_trait]
pub trait LiveHost: Send + Sync {
    /// How `stream` is split into topics — what the `publish` action needs to
    /// default a `user` stream's topic to the event's user, and to refuse
    /// saving a publish to a stream Saltcorn cannot publish to. An error for a
    /// stream that does not exist here or is not an `internal` one.
    fn topics(&self, stream: &str) -> Result<LiveTopics>;

    /// Publish `value` on `topic` of `stream`; `topic` is `None` for a stream
    /// without topics. Answers the topic as it was published (a user id in its
    /// canonical spelling).
    fn publish(&self, stream: &str, topic: Option<String>, value: Json) -> Result<Option<String>>;

    /// Read or edit the collaborative document of row `key` on the `document`
    /// stream `stream`, answering the document's plain text **after** the
    /// request. An edit is a CRDT update, so it merges with whatever the
    /// document's editors are typing, reaches them at once, and is persisted
    /// before this returns. Trusted, like a publish.
    async fn document(&self, stream: &str, key: &str, request: DocRequest) -> Result<String> {
        let _ = (key, request);
        Err(sc_error::Error::invalid(format!(
            "this server keeps no collaborative documents, so `{stream}` cannot be opened"
        )))
    }
}

/// What a code body asks of a collaborative document ([`LiveHost::document`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocRequest {
    /// Read its text.
    Read,
    /// On a `text` root: delete `delete` characters at `index` (UTF-16 code
    /// units, as JavaScript counts), then insert `insert` there.
    Edit {
        /// Where.
        index: u32,
        /// How many to delete.
        delete: u32,
        /// What to insert.
        insert: String,
    },
    /// Append `text`: to the end of a `text` root, or as a new paragraph of an
    /// `xml` root.
    Append {
        /// What to append.
        text: String,
    },
}

/// How a stream Saltcorn publishes to is split into topics, as a publisher
/// needs to know it ([`LiveHost::topics`]) — `sc-stream`'s `TopicSpec`, minus
/// the kind nothing publishes to by hand, in a crate that cannot name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveTopics {
    /// One topic: publish with none.
    Single,
    /// A topic per user id.
    User,
    /// A topic per row of `table`, named by the row's key.
    Row {
        /// The table whose rows the topics are.
        table: String,
    },
}

/// One argument of a module function, as v1 declared it.
///
/// v1's own `arguments: [{ name, type }]` vocabulary, carried rather than
/// reinvented — it is what lets a function's signature reach the code editor's
/// generated types (§12.2) without anybody inventing a second way to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFnArg {
    /// The argument's name, as the signature shows it.
    pub name: String,
    /// The v1 type name (`String`, `Integer`, `Object`), when declared.
    pub type_name: Option<String>,
}

/// One function a module supplies, as the guest needs to know it.
///
/// The name is **not** unique: two modules may each supply `geocode_lat`, and
/// nothing here pretends otherwise — which is exactly why the module is part of
/// the identity and why `modfn("@saltcorn/nominatim-geocode").geocode_lat` is
/// the spelling that always works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFunction {
    /// The package that supplies it.
    pub module: String,
    /// The function's own name, v1's and unqualified.
    pub name: String,
    /// The module's one-line description, if it gave one.
    pub description: String,
    /// Whether v1 itself treated it as awaitable. It decides nothing about how
    /// the call is made — everything crosses this seam awaited — only what a
    /// signature says.
    pub is_async: bool,
    /// The declared signature, when the module declared one.
    pub arguments: Vec<ModuleFnArg>,
}

/// The **fifth** host surface: one module function call in, one JSON value out
/// (§4a).
///
/// Separate from the other four for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no modules
/// installed), it is implemented somewhere else (over the module worker pool, in
/// `sc-module`), and it spends a budget of its own. What crosses is plain JSON,
/// so §15's other guest languages inherit `modfn` the way they inherit `db`:
///
/// ```json
/// { "module": "@saltcorn/nominatim-geocode", "function": "geocode_lat",
///   "args": [ { "street": "…", "city": "…" } ], "timeout_ms": 4750 }
/// ```
///
/// The plan always names a **module** as well as a function, because a function
/// name is not unique and the caller is the one who knows which it meant.
/// `timeout_ms` is filled in by the op from what is left of the run's wall
/// clock, so an implementation may take it as given.
///
/// **Why this is a hop at all**, since it is the question the seam invites: not
/// the isolate split. Every v1 function closes over something its module built
/// at load time — a `markdown-it`, a `Nominatim`, the module's own configuration
/// — and a module is loaded **once**, with one configuration, holding one set of
/// state, while code bodies run on a pool. So the function has to execute where
/// its module was loaded however the pools are arranged; merging them would mean
/// N geocoders, N configurations and N MQTT connections.
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a module that is not loaded, a function that threw, a result that will not
/// serialise — so a body may catch it and carry on.
#[async_trait]
pub trait ModuleFnHost: Send + Sync {
    /// Call one module function and answer what it returned.
    async fn call(&self, request: Json) -> Result<Json>;

    /// The functions to bind into the run's `modfn`, so `modfn.geocode_lat`
    /// resolves to one module **at once** rather than deferring the question to
    /// the call.
    ///
    /// Synchronous, for [`FileHost::store_names`]' reason: the guest's
    /// `modfn.x(…)` is one expression with nothing to await in the middle of it.
    /// A host that cannot enumerate without I/O answers with the default — an
    /// empty list, which the guest reads as "this server has no module
    /// functions" and refuses every name by saying so.
    fn functions(&self) -> Vec<ModuleFunction> {
        Vec::new()
    }
}

/// Two module hosts as one — the composite of §8, for a server whose modules are
/// written in more than one language.
///
/// A formula's hoisted call and a body's `modfn` reach *a* module function; that
/// one language's modules run on a Deno worker and another's on an embedded
/// interpreter is a fact about installation, not about calling. So the two hosts
/// are merged here, once, and everything above sees one — which is what keeps
/// `modfn.md_to_html(x)` from having to say which package manager put
/// `md_to_html` there.
///
/// Routing is by the **module** the plan names, which every plan carries because
/// a function name is not unique. A name two hosts both claim goes to the first
/// that has it, in the order they were given: the merge does not invent a
/// precedence rule, because a duplicate is already an issue on somebody's module
/// card and inventing one here would hide it.
pub struct ModuleFnHosts {
    hosts: Vec<Arc<dyn ModuleFnHost>>,
    functions: Vec<ModuleFunction>,
}

impl ModuleFnHosts {
    /// One host over all of them, in order.
    pub fn new(hosts: Vec<Arc<dyn ModuleFnHost>>) -> ModuleFnHosts {
        let functions = hosts.iter().flat_map(|host| host.functions()).collect();
        ModuleFnHosts { hosts, functions }
    }
}

#[async_trait]
impl ModuleFnHost for ModuleFnHosts {
    async fn call(&self, request: Json) -> Result<Json> {
        let module = request.get("module").and_then(Json::as_str).unwrap_or("");
        let function = request.get("function").and_then(Json::as_str).unwrap_or("");
        for host in &self.hosts {
            if host
                .functions()
                .iter()
                .any(|f| f.module == module && f.name == function)
            {
                return host.call(request).await;
            }
        }
        // Not silence, and not the first host's refusal: a name nothing supplies
        // is its own answer, and it is the same sentence either host would give.
        Err(Error::invalid(format!(
            "the module `{module}` supplies no function `{function}`"
        )))
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        self.functions.clone()
    }
}

/// A **guest language** that can run a [`CodeCall`]: JavaScript, Python, and
/// whatever comes third.
///
/// The trait exists because `CodeCall` was never JavaScript's — the source, the
/// bindings, the five borrowed host handles and the six budgets are the same
/// question in any language, and the host traits above were written for this
/// ("a Python or Rust adapter implements the same trait against the same
/// plans"). So a second language is a second implementation of *this* and
/// nothing below it: no second call type, no second host trait, and therefore
/// no way for two languages to disagree about authority, budgets or events.
///
/// Registered by [`language`](CodeAdapter::language) — `"python"` — so an
/// action reaches one as `ctx.adapter("python")` and the next guest language is
/// a registration rather than a field.
///
/// [`CodeRuntime`] is not required to implement it and does not: `run_js_code`
/// holds the concrete runtime, which is the thing that also carries a formula
/// evaluator's isolate pool. The trait is what the *other* languages are
/// reached through.
#[async_trait]
pub trait CodeAdapter: Send + Sync {
    /// The name this adapter is registered and configured under. Lower-case and
    /// stable: it is what an action's stored configuration says, so renaming it
    /// would strand every trigger already written against it.
    fn language(&self) -> &str;

    /// Run one code body to its JSON result, under the same bounds and against
    /// the same hosts a JavaScript body gets.
    async fn run_code(&self, call: CodeCall<'_>) -> Result<Json>;
}

/// The **schema snapshot** a guest run is handed: every table this server has,
/// with v1's property names on it, serialised once and stamped with the catalog
/// generation it was built at (TODO "the v1 `Table` API" §2).
///
/// It exists because v1's `Table.findOne` is **synchronous**. Eight years of
/// plugins are written as `const t = Table.findOne("books"); t.pk_name;` — not
/// awaited, on either line — and a host round trip cannot answer that. So the
/// metadata is in the isolate *before* the run starts and the division is v1's
/// own: metadata is local and synchronous, data is a host call and
/// asynchronous.
///
/// This crate does not know what a table is and does not learn here: what it
/// carries is the **text** and the **stamp**. Building it is the catalog's
/// side of the seam (`sc_api::code_host::schema`), reading it is the guest's,
/// and the stamp is what lets a run say "you already have this" in one integer
/// rather than in a megabyte of JSON — which is the whole reason a generation
/// exists rather than a hash.
///
/// Cheap to clone: the JSON is an [`Arc<str>`], so a snapshot crossing to a
/// worker thread copies sixteen bytes and a refcount rather than the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot {
    generation: u64,
    json: Arc<str>,
}

impl SchemaSnapshot {
    /// A snapshot of `json`, built at catalog generation `generation`.
    pub fn new(generation: u64, json: impl Into<Arc<str>>) -> SchemaSnapshot {
        SchemaSnapshot {
            generation,
            json: json.into(),
        }
    }

    /// The catalog generation this was built at — what a run carries, and what
    /// an isolate compares against what it already holds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The serialised schema, as the guest parses it.
    pub fn json(&self) -> &str {
        &self.json
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    /// A host of one module, answering with its own name so a test can tell
    /// which of two was asked.
    struct One {
        module: &'static str,
        function: &'static str,
    }

    #[async_trait]
    impl ModuleFnHost for One {
        async fn call(&self, request: Json) -> Result<Json> {
            Ok(Json::String(format!(
                "{} answered {}",
                self.module,
                request["function"].as_str().unwrap_or_default()
            )))
        }

        fn functions(&self) -> Vec<ModuleFunction> {
            vec![ModuleFunction {
                module: self.module.to_owned(),
                name: self.function.to_owned(),
                description: String::new(),
                is_async: false,
                arguments: Vec::new(),
            }]
        }
    }

    fn both() -> ModuleFnHosts {
        ModuleFnHosts::new(vec![
            Arc::new(One {
                module: "@saltcorn/markdown",
                function: "md_to_html",
            }),
            Arc::new(One {
                module: "sc-plugin-fixture",
                function: "fixture_shout",
            }),
        ])
    }

    #[tokio::test]
    async fn a_call_reaches_the_host_whose_module_supplies_it() {
        let hosts = both();
        // Every function of both, in one list — which is what a guest's `modfn`
        // resolves a short name against.
        let names: Vec<String> = hosts.functions().into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["md_to_html", "fixture_shout"]);

        let answer = hosts
            .call(serde_json::json!({
                "module": "sc-plugin-fixture", "function": "fixture_shout", "args": ["x"]
            }))
            .await
            .expect("the second host has it");
        assert_eq!(
            answer,
            Json::String("sc-plugin-fixture answered fixture_shout".into())
        );
    }

    #[tokio::test]
    async fn a_name_neither_supplies_is_refused_by_the_composite_itself() {
        let said = both()
            .call(serde_json::json!({ "module": "sc-plugin-fixture", "function": "nope" }))
            .await
            .expect_err("nothing supplies it")
            .to_string();
        // The sentence names both halves of the question, and it is the one
        // either host would have given — not "this host does not have it",
        // which would be true of half a server.
        assert!(
            said.contains("sc-plugin-fixture") && said.contains("nope"),
            "{said}"
        );
    }
}
