//! `feldspar agent …` and `feldspar build-agent`: agent evaluation and the build agent.

use sc_cli::DbConfig;
use sc_cli::{
    connect_catalog, connect_file_stores, connect_stored_databases, connect_stored_file_stores,
    extract_file_stores,
};
use sc_error::Result;

/// `feldspar build-app SUBDOMAIN [database flags] [--file-store NAME=PATH]`.
///
/// Builds one application from the command line, printing the tool output as it
/// goes and failing with the bundler's own diagnostics.
///
/// The admin UI can already build an app, and this does the same work — so why
/// have it? Because when a build fails, the UI shows the *result* and this shows
/// the *run*: it is scriptable, it is what a deploy step or a CI job calls, and
/// its output goes to a terminal where it can be piped, grepped and kept. It also
/// works when the app cannot be reached in a browser at all, which is precisely
/// the state a failing build tends to leave a deployment in.
///
/// Deliberately **builds without mounting**: nothing is served by this process,
/// so running it against a live deployment's database cannot disturb what that
/// server is serving. The next build or restart there picks up the output.
/// `feldspar agent eval <suite> [...]`.
///
/// The only `agent` subcommand for now, and the reason the group exists rather
/// than a top-level `eval`: what is evaluated is the agent, and anything else
/// worth doing to one from a terminal (listing them, running one) belongs beside
/// it rather than at the top of the binary's vocabulary.
pub(crate) async fn agent_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("eval") => agent_eval_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown agent subcommand `{other}`; the only one is `eval`"
        ))),
        None => Err(sc_error::Error::config(
            "usage: feldspar agent eval SUITE [--model provider/model] [database flags]",
        )),
    }
}

/// Run an eval suite against the models named on the command line (§13).
///
/// **This command spends money.** Nothing else in the workspace calls a vendor,
/// which is why the harness is a command and not a test: an operator asking for
/// an eval has decided to pay for one.
pub(crate) async fn agent_eval_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args.to_vec())?;
    let (file_store_specs, rest) = extract_file_stores(rest)?;
    let eval = sc_cli::eval::EvalArgs::parse(&rest)?;
    let mut tasks = sc_cli::eval::load_suite(&eval.suite)?;
    if !eval.only.is_empty() {
        let wanted = eval.only.clone();
        tasks.retain(|task| wanted.contains(&task.name));
        if tasks.is_empty() {
            return Err(sc_error::Error::config(format!(
                "no task of `{}` is named {}",
                eval.suite.display(),
                wanted.join(" or ")
            )));
        }
    }

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    connect_file_stores(&catalog, &file_store_specs)?;

    // The same host the server assembles: a task's agent is the builder agent,
    // whose `view_app` grant a host without a browser cannot honour.
    let browser = sc_server::detect_browser(None);
    if let Err(reason) = &browser {
        eprintln!("feldspar: view_app is unavailable: {reason}");
    }
    let agents =
        sc_server::install_agents_on(&catalog, sc_agent::HostCapabilities { browser }).await?;
    let registry = agents.registry().clone();
    let host =
        sc_cli::eval::EvalHost::new(&catalog, &registry).with_connector(agents.providers().clone());

    eprintln!(
        "feldspar: running {} task(s) of `{}`",
        tasks.len(),
        eval.suite.display()
    );
    let report = sc_cli::eval::run_suite(&host, &eval, &tasks).await?;
    let (json, markdown) = sc_cli::eval::write_report(&eval, &report)?;
    print!("{}", report.markdown());
    eprintln!(
        "feldspar: {} of {} passed; wrote {} and {}",
        report.passed(),
        report.tasks.len(),
        json.display(),
        markdown.display()
    );
    // A suite with a failing task is a failing command: an operator running this
    // in a script should not have to parse the report to find out.
    match report.passed() == report.tasks.len() {
        true => Ok(()),
        false => Err(sc_error::Error::config(format!(
            "{} of {} eval tasks failed",
            report.tasks.len() - report.passed(),
            report.tasks.len()
        ))),
    }
}

/// `feldspar build-agent --server URL [--token-file PATH] [--workspace DIR] [--insecure-http]
/// [--once]` — run this machine as a build host (`TODO-build-hosts.md`). See `sc_cli::build_agent`.
pub(crate) async fn build_agent_command(args: &[String]) -> Result<()> {
    sc_cli::build_agent::run(&sc_cli::build_agent::parse_args(args)?).await
}
