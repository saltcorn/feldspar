//! Stan TODO 11.1: the milestone's three models end to end against a **real
//! CmdStan**, through the assembled router — the file store holding the
//! program, the model saved over tables, Bind automatically, Preview data, a
//! fit that compiles and samples four chains, and the posterior read back by
//! label and key and written into the rows it is about.
//!
//! - **radon** (the definition of done): Gelman & Hill's varying-intercept
//!   model with a county-level predictor, over synthetic data drawn from known
//!   parameters — 85 counties, five with one home and one with none, 919
//!   homes — and, for milestone 31, the same model refitted by a workflow's
//!   `fit_model` and written back from its `run_js_code` step;
//! - **an AR(1) with gaps**, on a daily time grid with a 14-day horizon whose
//!   forecast comes back labelled with its dates and is inserted into a
//!   `forecasts` table;
//! - **a BYM2** over a 5 × 5 lattice of regions with a weekly random walk,
//!   bound from an adjacency junction table and a `cells` matrix.
//!
//! Every one is `#[ignore]`d, because it needs a built CmdStan. The CmdStan is
//! the one the server itself would discover — `$CMDSTAN`, else the newest
//! built `~/.cmdstan/cmdstan-*` — so after `feldspar cmdstan install` these
//! need nothing set:
//!
//! ```text
//! cargo test -p sc-server --test it -- --ignored stan_models
//! ```
//!
//! The data and the sampler both have fixed seeds, so a run is reproducible
//! for a given CmdStan: the assertions on the posterior are about *these*
//! draws. The iteration counts are small (500 warmup and 500 draws per chain,
//! more where the model needs them to converge), and the
//! compiled programs are cached in the system temporary directory between
//! runs, so a second run costs sampling alone.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, StanSettings, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers, start_workflow_engine,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const IGNORED: &str = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN";

/// The sampler's seed, for every fit here.
const SEED: u64 = 20_260_929;

// ---------------------------------------------------------------------------
// The programs, as the tutorial (docs/tutorial-stan.md) gives them.

/// §11's worked example.
const RADON: &str = r#"// Varying intercepts by county, with a county-level predictor
// (Gelman & Hill, ch. 12).
data {
  int<lower=1> N;                              // count(main)
  int<lower=1> J;                              // size(counties)
  array[N] int<lower=1, upper=J> county;       // index(main.county -> counties)
  vector[N] x;                                 // column(main.floor)
  vector[J] u;                                 // column(counties.log_uranium)
  vector[N] y;                                 // column(main.y)
}
parameters {
  real gamma0;
  real gamma1;
  real beta;
  real<lower=0> sigma_a;
  real<lower=0> sigma_y;
  vector[J] alpha_raw;
}
transformed parameters {
  vector[J] alpha = gamma0 + gamma1 * u + sigma_a * alpha_raw;   // labelled by county
}
model {
  alpha_raw ~ std_normal();
  gamma0 ~ normal(0, 5);
  gamma1 ~ normal(0, 5);
  beta ~ normal(0, 5);
  sigma_a ~ normal(0, 2);
  sigma_y ~ normal(0, 2);
  y ~ normal(alpha[county] + beta * x, sigma_y);
}
generated quantities {
  vector[N] log_lik;                                               // labelled by home
  for (n in 1:N) log_lik[n] = normal_lpdf(y[n] | alpha[county[n]] + beta * x[n], sigma_y);
}
"#;

/// An AR(1) about a mean, over a daily grid with gaps and a horizon: the
/// missing days are parameters, and the horizon is simulated forward.
const SALES: &str = r#"// A daily AR(1) with missing days and a forecast.
data {
  int<lower=2> T;                          // size(day): every day, the horizon included
  int<lower=0, upper=T - 2> H;             // size(day.future): the horizon
  vector[T] y;                             // series(main.amount over day, fill 0)
  array[T] int<lower=0, upper=1> seen;     // series_present(main.amount over day)
}
transformed data {
  int T0 = T - H;                          // the observed span
  int N_mis = T0 - sum(seen[1:T0]);
}
parameters {
  real mu;
  real<lower=-1, upper=1> phi;
  real<lower=0> sigma;
  vector[N_mis] y_mis;                     // the missing days
}
transformed parameters {
  vector[T0] level;
  {
    int m = 1;
    for (t in 1:T0) {
      if (seen[t]) {
        level[t] = y[t];
      } else {
        level[t] = y_mis[m];
        m += 1;
      }
    }
  }
}
model {
  mu ~ normal(0, 20);
  phi ~ normal(0, 0.5);
  sigma ~ normal(0, 5);
  level[1] ~ normal(mu, sigma / sqrt(1 - square(phi)));
  level[2:T0] ~ normal(mu + phi * (level[1:(T0 - 1)] - mu), sigma);
}
generated quantities {
  vector[H] y_future;                      // labelled with the horizon's dates
  {
    real previous = level[T0];
    for (h in 1:H) {
      y_future[h] = normal_rng(mu + phi * (previous - mu), sigma);
      previous = y_future[h];
    }
  }
}
"#;

/// BYM2 (Morris et al., 2019) for the regions, plus a random walk over the
/// weeks, for Poisson counts with an expected-count offset.
const OUTBREAK: &str = r#"// Weekly case counts per region: BYM2 in space, a random walk in time.
functions {
  real icar_normal_lpdf(vector phi, int N, array[] int node1, array[] int node2) {
    return -0.5 * dot_self(phi[node1] - phi[node2])
           + normal_lpdf(sum(phi) | 0, 0.001 * N);
  }
}
data {
  int<lower=1> R;                                // size(regions)
  int<lower=2> T;                                // size(week)
  int<lower=0> N_edges;                          // edge_count(adjacency)
  array[N_edges] int<lower=1, upper=R> node1;    // edge_from(adjacency)
  array[N_edges] int<lower=1, upper=R> node2;    // edge_to(adjacency)
  real<lower=0> scaling_factor;                  // icar_scale(adjacency)
  vector<lower=0>[R] E;                          // column(regions.expected)
  array[R, T] int<lower=0> y;                    // cells(main.cases, region x week)
}
transformed data {
  vector[R] log_E = log(E);
}
parameters {
  real beta0;
  real<lower=0> sigma;
  real<lower=0, upper=1> rho;
  vector[R] theta;
  vector[R] phi;
  real<lower=0> sigma_rw;
  vector[T - 1] rw_raw;
}
transformed parameters {
  vector[R] convolved_re = sqrt(1 - rho) * theta + sqrt(rho / scaling_factor) * phi;
  vector[T] rw;                                  // labelled by week
  {
    vector[T] walk = append_row(0, cumulative_sum(rw_raw * sigma_rw));
    rw = walk - mean(walk);
  }
}
model {
  for (t in 1:T) {
    y[:, t] ~ poisson_log(log_E + beta0 + convolved_re * sigma + rw[t]);
  }
  phi ~ icar_normal(R, node1, node2);
  theta ~ std_normal();
  beta0 ~ normal(0, 2);
  sigma ~ normal(0, 1);
  rho ~ beta(0.5, 0.5);
  sigma_rw ~ normal(0, 0.5);
  rw_raw ~ std_normal();
}
"#;

/// The tutorial's "Doing prediction in code": a new home's log radon, drawn
/// from the posterior predictive of the active Radon fit — draw `i` of every
/// variable comes from the same iteration, which is what makes combining them
/// draw by draw right.
const PREDICT_JS: &str = r#"
const m = await models.get("Radon");
const alpha = await m.draws("alpha", { keys: [row.county] });
const beta = await m.draws("beta");
const sigma = await m.draws("sigma_y");
const ys = [];
for (let c = 0; c < alpha.chains.length; c++) {
  const a = alpha.chains[c].draws[0], b = beta.chains[c].draws[0], s = sigma.chains[c].draws[0];
  for (let i = 0; i < a.length; i++) {
    const z = Math.sqrt(-2 * Math.log(1 - Math.random())) * Math.cos(2 * Math.PI * Math.random());
    ys.push(a[i] + b[i] * row.floor + s[i] * z);
  }
}
ys.sort((x, y) => x - y);
const at = (p) => ys[Math.floor(p * (ys.length - 1))];
await db.homes.where({ id: row.id }).update({
  predicted_mean: ys.reduce((t, y) => t + y, 0) / ys.length,
  predicted_q5: at(0.05),
  predicted_q95: at(0.95),
});
"#;

// ---------------------------------------------------------------------------
// Synthetic data from known parameters.

/// A small deterministic generator, so the data is the same every run.
struct Rng(u64);

impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    /// Knuth's method: fine for the small means here.
    fn poisson(&mut self, mean: f64) -> u64 {
        let limit = (-mean).exp();
        let (mut k, mut p) = (0, 1.0);
        loop {
            p *= self.uniform();
            if p <= limit {
                return k;
            }
            k += 1;
        }
    }
}

/// The radon truth: what the posterior should recover.
const GAMMA0: f64 = 1.5;
const GAMMA1: f64 = 0.7;
const BETA: f64 = -0.7;
const SIGMA_A: f64 = 0.3;
const SIGMA_Y: f64 = 0.75;
const COUNTIES: usize = 85;
const HOMES: usize = 919;

/// A county's key: Minnesota's FIPS codes are odd, from 27001.
fn fips(k: usize) -> i64 {
    27_001 + 2 * k as i64
}

/// How many homes county `k` has: five with one, the last with none, the
/// sixth large enough to make 919, and the rest between 2 and 18.
fn homes_in(k: usize) -> usize {
    let rest: usize = (6..COUNTIES - 1).map(|k| 2 + (k * 37) % 17).sum();
    match k {
        0..5 => 1,
        5 => HOMES - 5 - rest,
        k if k == COUNTIES - 1 => 0,
        k => 2 + (k * 37) % 17,
    }
}

/// `counties` and `homes`, drawn from the truth above.
fn radon_sql() -> String {
    let mut rng = Rng(11);
    let mut sql = String::from(
        "CREATE TABLE counties (
            id bigint primary key, name text, log_uranium double precision,
            alpha_mean double precision, alpha_sd double precision
         );
         CREATE TABLE homes (
            id bigint primary key, county bigint references counties(id),
            floor double precision, log_radon double precision,
            predicted_mean double precision, predicted_q5 double precision,
            predicted_q95 double precision
         );\n",
    );
    let mut home = 0;
    for k in 0..COUNTIES {
        let u = -0.2 + 0.35 * rng.normal();
        let alpha = GAMMA0 + GAMMA1 * u + SIGMA_A * rng.normal();
        sql.push_str(&format!(
            "INSERT INTO counties (id, name, log_uranium) VALUES ({}, 'County {:02}', {u});\n",
            fips(k),
            k + 1
        ));
        for _ in 0..homes_in(k) {
            home += 1;
            let floor = if rng.uniform() < 0.2 { 1.0 } else { 0.0 };
            let y = alpha + BETA * floor + SIGMA_Y * rng.normal();
            sql.push_str(&format!(
                "INSERT INTO homes (id, county, floor, log_radon) VALUES ({home}, {}, {floor}, {y});\n",
                fips(k)
            ));
        }
    }
    assert_eq!(home, HOMES);
    sql
}

/// The AR(1) truth.
const MU: f64 = 10.0;
const PHI: f64 = 0.7;
const SIGMA: f64 = 1.0;
/// Days observed, from 2025-01-01; the gaps are inside them.
const DAYS: usize = 120;
const HORIZON: usize = 14;

/// Whether day `d` (0-based) is missing: eleven scattered days, one of them a
/// run of three.
fn missing(d: usize) -> bool {
    matches!(d, 9 | 23 | 40 | 41 | 42 | 57 | 71 | 88 | 95 | 104 | 113)
}

/// `sales(day, amount)` and an empty `forecasts` table.
fn sales_sql() -> String {
    let mut rng = Rng(23);
    let mut sql = String::from(
        "CREATE TABLE sales (id bigint primary key, day date, amount double precision);
         CREATE TABLE forecasts (
            id bigint generated by default as identity primary key,
            day date, mean double precision, lower double precision, upper double precision,
            fit text
         );\n",
    );
    let mut level = MU + SIGMA / (1.0 - PHI * PHI).sqrt() * rng.normal();
    for d in 0..DAYS {
        if d > 0 {
            level = MU + PHI * (level - MU) + SIGMA * rng.normal();
        }
        if !missing(d) {
            sql.push_str(&format!(
                "INSERT INTO sales VALUES ({}, DATE '2025-01-01' + {d}, {level});\n",
                d + 1
            ));
        }
    }
    sql
}

/// The outbreak truth.
const BETA0: f64 = 0.5;
const SIDE: usize = 5;
const WEEKS: usize = 12;

/// `regions` on a 5 × 5 lattice, `region_adjacency` with each rook pair once,
/// and `cases` per region per week (Mondays from 2025-01-06).
fn outbreak_sql() -> String {
    let mut rng = Rng(37);
    let mut sql = String::from(
        "CREATE TABLE regions (id bigint primary key, name text, expected double precision);
         CREATE TABLE region_adjacency (
            id bigint generated by default as identity primary key,
            a bigint references regions(id), b bigint references regions(id)
         );
         CREATE TABLE cases (
            id bigint generated by default as identity primary key,
            region bigint references regions(id), week date, cases integer
         );\n",
    );
    let id = |r: usize, c: usize| (r * SIDE + c + 1) as i64;
    // A smooth north–south gradient plus a little noise, on the log scale.
    let (mut effect, mut expected, mut edges) = (Vec::new(), Vec::new(), String::new());
    for r in 0..SIDE {
        for c in 0..SIDE {
            expected.push(5.0 + 15.0 * rng.uniform());
            effect.push(0.25 * (r as f64 - 2.0) + 0.1 * rng.normal());
            sql.push_str(&format!(
                "INSERT INTO regions VALUES ({}, 'R{}{}', {});\n",
                id(r, c),
                r + 1,
                c + 1,
                expected[expected.len() - 1]
            ));
            if c + 1 < SIDE {
                edges.push_str(&format!(
                    "INSERT INTO region_adjacency (a, b) VALUES ({}, {});\n",
                    id(r, c),
                    id(r, c + 1)
                ));
            }
            if r + 1 < SIDE {
                edges.push_str(&format!(
                    "INSERT INTO region_adjacency (a, b) VALUES ({}, {});\n",
                    id(r, c),
                    id(r + 1, c)
                ));
            }
        }
    }
    sql.push_str(&edges);
    let mut walk = vec![0.0];
    for _ in 1..WEEKS {
        walk.push(walk.last().copied().unwrap_or(0.0) + 0.15 * rng.normal());
    }
    let centre = walk.iter().sum::<f64>() / WEEKS as f64;
    for (region, e) in expected.iter().enumerate() {
        for (week, w) in walk.iter().enumerate() {
            let mean = e * (BETA0 + effect[region] + w - centre).exp();
            sql.push_str(&format!(
                "INSERT INTO cases (region, week, cases) VALUES \
                 ({}, DATE '2025-01-06' + {}, {});\n",
                region + 1,
                7 * week,
                rng.poisson(mean)
            ));
        }
    }
    sql
}

// ---------------------------------------------------------------------------
// The server.

struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, HashMap<String, String>, Vec<u8>) {
        let body = match body {
            Some(mut b) if crate::named_datasets::carries_a_model(method, path) => {
                let mut ids = Vec::new();
                for (pointer, create) in crate::named_datasets::inline_datasets(&b) {
                    let (status, _, made) =
                        Box::pin(self.raw("POST", "/api/datasets", Some(create))).await;
                    let made: Value = serde_json::from_slice(&made).unwrap_or(Value::Null);
                    assert!(status.is_success(), "creating a dataset: {status} {made}");
                    ids.push((pointer, made["dataset"]["id"].as_str().unwrap().to_owned()));
                }
                crate::named_datasets::use_ids(&mut b, ids);
                Some(b)
            }
            other => other,
        };
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let mut headers = HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(text) = value.to_str() {
                headers.insert(name.as_str().to_owned(), text.to_owned());
            }
        }
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, _, bytes) = self.raw(method, path, body).await;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    /// Save a model and fit it to the end — a compile is a minute the first
    /// time — answering the finished instance.
    async fn fit(&mut self, model: Value) -> Value {
        let saved = self.ok("POST", "/api/models", Some(model)).await;
        let id = saved["id"].as_str().unwrap().to_owned();
        let started = self
            .ok("POST", &format!("/api/models/{id}/fit"), Some(json!({})))
            .await;
        let instance = started["id"].as_str().unwrap().to_owned();
        for _ in 0..2_400 {
            let body = self
                .ok("GET", &format!("/api/model-instances/{instance}"), None)
                .await;
            if body["status"] != json!("fitting") {
                assert_eq!(body["status"], json!("fitted"), "{body}");
                return body;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("the fit of instance {instance} did not finish in ten minutes");
    }

    /// The summary of `variable`, as a list of rows keyed by column name.
    async fn summary(&mut self, instance: &str, variable: &str) -> Vec<HashMap<String, Value>> {
        let body = self
            .ok(
                "GET",
                &format!("/api/model-instances/{instance}/summary?variable={variable}"),
                None,
            )
            .await;
        let columns: Vec<String> = body["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_owned())
            .collect();
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .cloned()
                    .zip(row.as_array().unwrap().iter().cloned())
                    .collect()
            })
            .collect()
    }

    async fn rows(&mut self, table: &str) -> Vec<Value> {
        self.ok("GET", &format!("/api/tables/{table}/rows"), None)
            .await
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn number(row: &HashMap<String, Value>, column: &str) -> f64 {
    row.get(column)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("no number `{column}` in {row:?}"))
}

/// Whether `truth` is inside the summary row's 90 % interval.
fn covers(row: &HashMap<String, Value>, truth: f64) -> bool {
    number(row, "q5") <= truth && truth <= number(row, "q95")
}

struct Server {
    client: Client,
    /// The file store's directory, holding the programs and the raw runs.
    store: PathBuf,
    _engine: Arc<sc_workflow::WorkflowEngineTask>,
    _db: TestDb,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store);
    }
}

/// A server over `schema`, with a file store `stan` holding `programs`, and
/// its Stan provider ready — or a panic saying why not.
async fn setup(label: &str, schema: &str, programs: &[(&str, &str)]) -> Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(schema)
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let store = std::env::temp_dir().join(format!("sc-stan-models-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    for (path, text) in programs {
        let file = store.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    // A cache that outlives the run, so the second run does not recompile.
    let stan = StanSettings {
        cache_dir: Some(std::env::temp_dir().join("sc-stan-models-cache")),
        ..StanSettings::default()
    };
    let models =
        sc_server::install_models_with(&catalog, sc_model::DEFAULT_MAX_ROWS, &stan).await?;
    if let Some(why) = models.stan().unavailable() {
        panic!("{why}\n({IGNORED})");
    }
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    // For a workflow that refits and then writes back from a code step; only
    // `serve` starts the engine, and this stands in for it.
    let (engine, _handle) = start_workflow_engine(&catalog, &dispatcher);
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_triggers(dispatcher.clone())
            .with_models(models.clone()),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.raw("GET", "/api/auth/status", None).await;
    client
        .ok(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    client
        .ok(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "stan", "description": "Stan programs and runs", "backend": "local",
                "config": { "path": store.to_string_lossy() }, "min_role": 1,
            })),
        )
        .await;
    Ok(Server {
        client,
        store,
        _engine: engine,
        _db: db,
    })
}

/// The sampler settings every fit here shares.
fn sampler(program: &str) -> serde_json::Map<String, Value> {
    json!({
        "program_store": "stan", "program": program,
        "chains": 4, "iter_warmup": 500, "iter_sampling": 500, "seed": SEED,
    })
    .as_object()
    .cloned()
    .unwrap()
}

// ---------------------------------------------------------------------------
// Radon: the definition of done.

fn radon_model(bindings: Value) -> Value {
    let mut configuration = sampler("models/radon.stan");
    configuration.insert("bindings".into(), bindings);
    configuration.insert("runs_store".into(), json!("stan"));
    // The definition of done's 4 × 1 000: at 500, a few of the 85 R̂s land
    // just over 1.01 by chance.
    configuration.insert("iter_sampling".into(), json!(1000));
    json!({
        "name": "Radon",
        "description": "Varying intercepts by county",
        "provider": "stan",
        "dataset": {
            "table": "homes",
            "columns": [
                { "name": "county", "expr": "county" },
                { "name": "floor", "expr": "floor" },
                { "name": "y", "expr": "log_radon" },
            ],
        },
        "related": [{
            "name": "counties",
            "dataset": {
                "table": "counties",
                "columns": [{ "name": "log_uranium", "expr": "log_uranium" }],
            },
            "label": "name",
        }],
        "configuration": configuration,
        "hyperparameters": {},
        "attributes": {},
    })
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn radon_is_bound_sampled_labelled_by_county_and_written_back() -> Result<()> {
    let mut server = setup("radon", &radon_sql(), &[("models/radon.stan", RADON)]).await?;
    let client = &mut server.client;

    // Bind automatically: `N`, `J`, `county` and `y` from the names, the
    // foreign key and the size expressions; `x` and `u` are the admin's.
    let suggested = client
        .ok(
            "POST",
            "/api/model-bindings/suggest",
            Some(radon_model(json!({}))),
        )
        .await;
    let mut bindings = suggested["bindings"].clone();
    assert_eq!(
        bindings,
        json!({
            "N": { "kind": "count", "dataset": "main" },
            "J": { "kind": "size", "dimension": "counties" },
            "county": { "kind": "index", "dataset": "main", "column": "county",
                        "dimension": "counties" },
            "y": { "kind": "column", "dataset": "main", "column": "y" },
        }),
        "{suggested}"
    );
    bindings["x"] = json!({ "kind": "column", "dataset": "main", "column": "floor" });
    bindings["u"] = json!({ "kind": "column", "dataset": "counties", "column": "log_uranium" });

    // Preview data: 919 homes, 85 counties, everything bound.
    let preview = client
        .ok(
            "POST",
            "/api/model-data/preview",
            Some(radon_model(bindings.clone())),
        )
        .await;
    assert_eq!(preview["errors"], json!([]), "{preview}");
    assert_eq!(preview["report"]["dimensions"]["main"], json!(HOMES));
    assert_eq!(preview["report"]["dimensions"]["counties"], json!(COUNTIES));
    let first = |name: &str| {
        preview["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == json!(name))
            .map(|v| v["first"].clone())
            .unwrap()
    };
    assert_eq!(first("N"), json!([HOMES]), "{preview}");
    assert_eq!(first("J"), json!([COUNTIES]), "{preview}");

    // Fit: four chains, no warnings.
    let instance = client.fit(radon_model(bindings)).await;
    assert_eq!(instance["warnings"], json!([]), "{instance}");
    assert_eq!(instance["program_changed"], json!(false), "{instance}");
    let id = instance["id"].as_str().unwrap().to_owned();
    let metrics = instance["metrics"].to_string();
    let train = &instance["metrics"]["train"];
    assert_eq!(train["divergent"], json!(0), "{metrics}");
    assert!(train["max_rhat"].as_f64().unwrap() <= 1.01, "{metrics}");
    assert_eq!(
        instance["variables"]["alpha"],
        json!({ "dims": [COUNTIES], "dimensions": ["counties"] }),
        "{instance}"
    );

    // The population parameters are recovered.
    for (name, truth) in [("gamma0", GAMMA0), ("gamma1", GAMMA1), ("beta", BETA)] {
        let row = &client.summary(&id, name).await[0];
        assert!(covers(row, truth), "{name} = {truth} is outside {row:?}");
    }

    // `alpha` is one row per county, labelled by name; the county with no
    // homes has a wider interval than the median county.
    let alpha = client.summary(&id, "alpha").await;
    assert_eq!(alpha.len(), COUNTIES);
    assert_eq!(alpha[0]["counties"], json!("County 01"));
    assert_eq!(alpha[COUNTIES - 1]["counties"], json!("County 85"));
    let width = |row: &HashMap<String, Value>| number(row, "q95") - number(row, "q5");
    let mut widths: Vec<f64> = alpha.iter().map(width).collect();
    let empty = widths[COUNTIES - 1];
    widths.sort_by(f64::total_cmp);
    let median = widths[COUNTIES / 2];
    assert!(
        empty > 1.5 * median,
        "the empty county's interval ({empty}) is not visibly wider than the median ({median})"
    );

    // Its draws, keyed by county id: 4 × 1 000.
    let draws = client
        .ok(
            "GET",
            &format!("/api/model-instances/{id}/draws?variable=alpha"),
            None,
        )
        .await;
    assert_eq!(draws["dims"], json!([COUNTIES]));
    let keys: Vec<i64> = (0..COUNTIES).map(fips).collect();
    assert_eq!(draws["keys"], json!([keys]));
    assert_eq!(draws["chains"].as_array().unwrap().len(), 4);
    for chain in draws["chains"].as_array().unwrap() {
        let elements = chain["draws"].as_array().unwrap();
        assert_eq!(elements.len(), COUNTIES);
        assert!(elements.iter().all(|e| e.as_array().unwrap().len() == 1000));
    }

    // Write back the mean and sd into `counties`.
    let written = client
        .ok(
            "POST",
            &format!("/api/model-instances/{id}/posterior-writes"),
            Some(json!({
                "variable": "alpha", "mode": "update",
                "statistics": { "mean": "alpha_mean", "sd": "alpha_sd" },
            })),
        )
        .await;
    assert_eq!(written["written"], json!(COUNTIES), "{written}");
    let counties = client.rows("counties").await;
    let sd = |key: i64| {
        counties
            .iter()
            .find(|c| c["id"] == json!(key))
            .and_then(|c| c["alpha_sd"].as_f64())
            .unwrap()
    };
    assert!(counties.iter().all(|c| c["alpha_mean"].is_f64()));
    assert!(sd(fips(COUNTIES - 1)) > sd(fips(5)));

    // Prediction in code: a home inserted into the county with none gets the
    // posterior predictive's mean and interval from its trigger.
    client
        .ok("POST", &format!("/api/model-instances/{id}/activate"), None)
        .await;
    client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "predict_radon", "description": "", "when": "insert",
                "channel": "homes", "only_if": null, "action": "run_js_code",
                "configuration": { "code": PREDICT_JS }, "min_role": null, "enabled": true,
            })),
        )
        .await;
    client
        .ok(
            "POST",
            "/api/tables/homes/rows",
            Some(json!({ "id": 10_000, "county": fips(COUNTIES - 1), "floor": 0.0 })),
        )
        .await;
    let home = client
        .rows("homes")
        .await
        .into_iter()
        .find(|h| h["id"] == json!(10_000))
        .unwrap();
    let mean = home["predicted_mean"]
        .as_f64()
        .unwrap_or_else(|| panic!("{home}"));
    let empty = &alpha[COUNTIES - 1];
    assert!(
        (mean - number(empty, "mean")).abs() < 0.1,
        "{home} {empty:?}"
    );
    // The interval is the county's uncertainty and a home's own spread.
    let (q5, q95) = (
        home["predicted_q5"].as_f64().unwrap(),
        home["predicted_q95"].as_f64().unwrap(),
    );
    assert!(q95 - q5 > 2.0 * 1.645 * SIGMA_Y * 0.9, "{home}");
    assert!(q5 < mean && mean < q95, "{home}");

    // Download run: CmdStan's own CSVs, as `cmdstanpy.from_csv` reads them,
    // beside the program, the data and the coordinates.
    let (status, _, zip) = client
        .raw("GET", &format!("/api/model-instances/{id}/run"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    for expected in [
        "chain-1.csv",
        "chain-4.csv",
        "data.json",
        "coordinates.json",
        "config.json",
        "program/models/radon.stan",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} is not in {names:?}"
        );
    }
    let mut csv = String::new();
    archive
        .by_name("chain-1.csv")
        .unwrap()
        .read_to_string(&mut csv)
        .unwrap();
    assert!(csv.starts_with("# stan_version_major"), "{}", &csv[..200]);
    assert!(
        csv.lines()
            .any(|l| l.starts_with("lp__,accept_stat__,stepsize__")),
        "no header"
    );
    Ok(())
}

/// Milestone 31's Radon half, against a real CmdStan: a workflow refits the
/// model with `fit_model`, and a `run_js_code` step writes the new fit's
/// posterior back through `models.get("Radon")`, firing the `counties` update
/// trigger once per county. The same body learns that a posterior does not
/// predict rows. (`models_without_actions.rs` walks the same over the stub
/// sampler.)
#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn radon_is_refitted_by_a_workflow_and_written_back_from_code() -> Result<()> {
    let schema = format!(
        "{}\nCREATE TABLE audit (id bigint generated by default as identity primary key, \
         what text);\n\
         CREATE TABLE refits (id bigint generated by default as identity primary key, \
         note text);\n",
        radon_sql()
    );
    let mut server = setup("radon-workflow", &schema, &[("models/radon.stan", RADON)]).await?;
    let client = &mut server.client;
    let mut bindings = client
        .ok(
            "POST",
            "/api/model-bindings/suggest",
            Some(radon_model(json!({}))),
        )
        .await["bindings"]
        .clone();
    bindings["x"] = json!({ "kind": "column", "dataset": "main", "column": "floor" });
    bindings["u"] = json!({ "kind": "column", "dataset": "counties", "column": "log_uranium" });
    client
        .ok("POST", "/api/models", Some(radon_model(bindings)))
        .await;
    client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "audit_counties", "description": "", "when": "update",
                "channel": "counties", "only_if": null, "action": "insert_row",
                "configuration": { "table": "audit", "values": { "what": "row.name" } },
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    let created = client
        .ok(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "radon_nightly", "description": "", "when": "insert",
                "channel": "refits", "only_if": null, "body": "workflow", "action": null,
                "configuration": null, "min_role": null, "enabled": true,
            })),
        )
        .await;
    let workflow = created["id"].as_str().unwrap().to_owned();
    let saved = client
        .ok(
            "POST",
            &format!("/api/workflows/{workflow}"),
            Some(json!({
                "description": "refit Radon, then write it back",
                "workflow": {
                    "start": "refit",
                    "steps": [
                        {
                            "name": "refit",
                            "kind": { "type": "action", "action": "fit_model",
                                      "configuration": { "model": "Radon",
                                                         "activate": "if_clean" } },
                            "next": { "type": "step", "step": "write_back" }
                        },
                        {
                            "name": "write_back",
                            "kind": { "type": "action", "action": "run_js_code",
                                      "configuration": { "timeout_ms": 60000, "code": r#"
                                const m = await models.get("Radon");
                                const w = await m.writePosterior({ variable: "alpha",
                                    statistics: { mean: "alpha_mean", sd: "alpha_sd" } });
                                let predicted;
                                try { await m.predict({ id: 1 }); }
                                catch (e) { predicted = e.message; }
                                return { written: w.written, fit: m.fit.id,
                                         refit: context.refit, predicted };
                            "# } },
                            "next": { "type": "end" }
                        }
                    ]
                },
            })),
        )
        .await;
    assert_eq!(saved["issues"], json!([]), "{saved}");
    client
        .ok(
            "POST",
            "/api/tables/refits/rows",
            Some(json!({ "note": "nightly" })),
        )
        .await;

    // Compiling (unless cached) and sampling 4 × 1 500 iterations.
    let mut run = Value::Null;
    for _ in 0..2400 {
        let runs = client
            .ok("GET", &format!("/api/workflows/{workflow}/runs"), None)
            .await;
        if let Some(first) = runs.as_array().and_then(|r| r.first())
            && ["done", "failed", "aborted"].contains(&first["state"].as_str().unwrap_or(""))
        {
            let id = first["id"].as_str().unwrap().to_owned();
            run = client.ok("GET", &format!("/api/runs/{id}"), None).await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(run["state"], json!("done"), "{run}");
    let wrote = &run["context"]["context"]["write_back"];
    assert_eq!(wrote["refit"]["warnings"], json!([]), "{run}");
    assert_eq!(wrote["refit"]["active"], json!(true), "{run}");
    assert_eq!(wrote["fit"], wrote["refit"]["instance"], "{run}");
    assert_eq!(wrote["written"], json!(COUNTIES), "{run}");
    assert!(
        wrote["predicted"]
            .as_str()
            .is_some_and(|p| p.contains("`m.draws(…)`")),
        "{run}"
    );
    let counties = client.rows("counties").await;
    assert!(counties.iter().all(|c| c["alpha_mean"].is_f64()));
    assert_eq!(client.rows("audit").await.len(), COUNTIES);
    Ok(())
}

// ---------------------------------------------------------------------------
// A daily AR(1) with gaps, and a forecast.

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn an_ar1_with_gaps_forecasts_fourteen_days_labelled_with_their_dates() -> Result<()> {
    let mut server = setup("sales", &sales_sql(), &[("models/sales.stan", SALES)]).await?;
    let client = &mut server.client;

    let mut configuration = sampler("models/sales.stan");
    configuration.insert(
        "dimensions".into(),
        json!({ "day": { "kind": "time_grid", "dataset": "main", "column": "day",
                         "step": "day", "horizon": HORIZON } }),
    );
    configuration.insert(
        "bindings".into(),
        json!({
            "T": { "kind": "size", "dimension": "day" },
            "H": { "kind": "size", "dimension": "day.future" },
            "y": { "kind": "series", "dataset": "main", "column": "amount",
                   "over": { "dimension": "day" }, "fill": 0 },
            "seen": { "kind": "series_present", "dataset": "main", "column": "amount",
                      "over": { "dimension": "day" } },
        }),
    );
    let model = json!({
        "name": "Sales",
        "description": "Daily sales, an AR(1) with a forecast",
        "provider": "stan",
        "dataset": {
            "table": "sales",
            "columns": [
                { "name": "day", "expr": "day" },
                { "name": "amount", "expr": "amount" },
            ],
            "order": [{ "expr": "day", "descending": false }],
        },
        "configuration": configuration,
        "hyperparameters": {},
        "attributes": {},
    });

    let preview = client
        .ok("POST", "/api/model-data/preview", Some(model.clone()))
        .await;
    assert_eq!(preview["errors"], json!([]), "{preview}");
    assert_eq!(
        preview["report"]["dimensions"]["day"],
        json!(DAYS + HORIZON),
        "{preview}"
    );

    let instance = client.fit(model).await;
    assert_eq!(instance["warnings"], json!([]), "{instance}");
    let id = instance["id"].as_str().unwrap().to_owned();
    for (name, truth) in [("mu", MU), ("phi", PHI), ("sigma", SIGMA)] {
        let row = &client.summary(&id, name).await[0];
        assert!(covers(row, truth), "{name} = {truth} is outside {row:?}");
    }
    // The eleven missing days are estimated.
    assert_eq!(client.summary(&id, "y_mis").await.len(), 11);

    // The forecast: fourteen days after 2025-04-30, by date.
    let forecast = client.summary(&id, "y_future").await;
    let days: Vec<&str> = forecast
        .iter()
        .map(|r| r["day.future"].as_str().unwrap())
        .collect();
    assert_eq!(days.len(), HORIZON);
    assert_eq!(days[0], "2025-05-01");
    assert_eq!(days[HORIZON - 1], "2025-05-14");
    // It reverts towards the mean, and grows less certain as it goes.
    let last = forecast.last().unwrap();
    assert!((number(last, "mean") - MU).abs() < 1.5, "{last:?}");
    assert!(number(last, "sd") > number(&forecast[0], "sd"));

    // …and is written into `forecasts`, one row per day.
    let written = client
        .ok(
            "POST",
            &format!("/api/model-instances/{id}/posterior-writes"),
            Some(json!({
                "variable": "y_future", "mode": "insert", "table": "forecasts",
                "statistics": { "mean": "mean", "q5": "lower", "q95": "upper" },
                "coordinates": [{ "axis": "day.future", "field": "day" }],
                "instance_field": "fit",
            })),
        )
        .await;
    assert_eq!(written["written"], json!(HORIZON), "{written}");
    let rows = client.rows("forecasts").await;
    assert_eq!(rows.len(), HORIZON);
    let first = rows
        .iter()
        .find(|r| {
            r["day"]
                .as_str()
                .is_some_and(|d| d.starts_with("2025-05-01"))
        })
        .unwrap_or_else(|| panic!("no forecast for 2025-05-01 in {rows:?}"));
    assert_eq!(first["fit"], json!(id));
    assert!(first["lower"].as_f64().unwrap() < first["upper"].as_f64().unwrap());
    Ok(())
}

// ---------------------------------------------------------------------------
// BYM2 in space, a random walk in time.

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn a_bym2_over_a_lattice_with_a_weekly_random_walk() -> Result<()> {
    let mut server = setup(
        "outbreak",
        &outbreak_sql(),
        &[("models/outbreak.stan", OUTBREAK)],
    )
    .await?;
    let client = &mut server.client;

    let edges = |kind: &str| {
        json!({ "kind": kind, "dataset": "adjacency", "from": "a", "to": "b",
                "dimension": "regions" })
    };
    let mut configuration = sampler("models/outbreak.stan");
    // `sigma` and `rho` share out one variance between two terms, and mix
    // slowly: at 500 draws their R̂ is 1.016 and their bulk ESS under 400.
    configuration.insert("iter_warmup".into(), json!(1000));
    configuration.insert("iter_sampling".into(), json!(1000));
    configuration.insert(
        "dimensions".into(),
        json!({ "week": { "kind": "time_grid", "dataset": "main", "column": "week",
                          "step": "week" } }),
    );
    configuration.insert(
        "bindings".into(),
        json!({
            "R": { "kind": "size", "dimension": "regions" },
            "T": { "kind": "size", "dimension": "week" },
            "N_edges": edges("edge_count"),
            "node1": edges("edge_from"),
            "node2": edges("edge_to"),
            "scaling_factor": edges("icar_scale"),
            "E": { "kind": "column", "dataset": "regions", "column": "expected" },
            "y": { "kind": "cells", "dataset": "main", "column": "cases",
                   "rows": { "dimension": "regions", "column": "region" },
                   "cols": { "dimension": "week" } },
        }),
    );
    let model = json!({
        "name": "Outbreak",
        "description": "Weekly cases: BYM2 in space, a random walk in time",
        "provider": "stan",
        "dataset": {
            "table": "cases",
            "columns": [
                { "name": "region", "expr": "region" },
                { "name": "week", "expr": "week" },
                { "name": "cases", "expr": "cases" },
            ],
        },
        "related": [
            {
                "name": "regions",
                "dataset": {
                    "table": "regions",
                    "columns": [{ "name": "expected", "expr": "expected" }],
                },
                "label": "name",
            },
            {
                "name": "adjacency",
                "dataset": {
                    "table": "region_adjacency",
                    "columns": [
                        { "name": "a", "expr": "a" },
                        { "name": "b", "expr": "b" },
                    ],
                },
            },
        ],
        "configuration": configuration,
        "hyperparameters": {},
        "attributes": {},
    });

    let preview = client
        .ok("POST", "/api/model-data/preview", Some(model.clone()))
        .await;
    assert_eq!(preview["errors"], json!([]), "{preview}");
    // Every region has a neighbour, so nothing is warned about.
    assert!(
        preview["report"]["warnings"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{preview}"
    );
    let first = |name: &str| {
        preview["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == json!(name))
            .map(|v| v["first"].clone())
            .unwrap()
    };
    // A 5 × 5 rook lattice has 2 × 5 × 4 = 40 edges.
    assert_eq!(first("N_edges"), json!([40]), "{preview}");
    assert_eq!(first("T"), json!([WEEKS]), "{preview}");

    let instance = client.fit(model).await;
    let id = instance["id"].as_str().unwrap().to_owned();
    assert_eq!(instance["warnings"], json!([]), "{instance}");
    let beta0 = &client.summary(&id, "beta0").await[0];
    assert!(covers(beta0, BETA0), "beta0 = {BETA0} is outside {beta0:?}");

    // The spatial effects are labelled by region, the walk by week.
    let convolved = client.summary(&id, "convolved_re").await;
    assert_eq!(convolved.len(), SIDE * SIDE);
    assert_eq!(convolved[0]["regions"], json!("R11"));
    // The gradient runs north to south.
    assert!(number(&convolved[0], "mean") < number(&convolved[SIDE * SIDE - 1], "mean"));
    let rw = client.summary(&id, "rw").await;
    let weeks: Vec<&str> = rw.iter().map(|r| r["week"].as_str().unwrap()).collect();
    assert_eq!(weeks.len(), WEEKS);
    assert_eq!(weeks[0], "2025-01-06");
    assert_eq!(weeks[WEEKS - 1], "2025-03-24");
    Ok(())
}
