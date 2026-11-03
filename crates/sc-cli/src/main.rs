//! The feldspar binary: serve and management commands (layer 10).
//!
//! The entry point runs on the **tokio** runtime (the workspace-wide runtime
//! decision), since the server it launches is async top to bottom. It exposes
//! the `serve` command, which connects the primary database, initialises the
//! [`Catalog`](sc_catalog::Catalog), and starts the HTTP server with the admin
//! API mounted. The database connection is configured by [`DbConfig`](sc_cli::DbConfig) — flags,
//! the environment, or the environment selected with `--environment` out of the
//! `feldspar.toml` configuration file; the remaining flags configure the HTTP
//! server itself ([`ServerConfig`](sc_server::ServerConfig)). The reusable boot logic lives in the crate
//! library ([`sc_cli`]); each subcommand is a module of [`commands`].

mod commands;

use std::process::ExitCode;

use commands::agent::{agent_command, build_agent_command};
use commands::api::{api_command, build_app_command};
use commands::app::app_command;
use commands::auth::auth_command;
use commands::backup::{backup_command, restore_command};
use commands::check::check_command;
use commands::cmdstan::cmdstan_command;
use commands::config::{get_cfg_command, send_email_command, set_cfg_command};
use commands::demo::demo_command;
use commands::i18n::i18n_command;
use commands::mcp_token::mcp_token_command;
use commands::migrate::migrate_command;
use commands::module::module_command;
use commands::python::python_command;
use commands::serve::serve_command;
use commands::users::{add_user_command, delete_user_command, modify_user_command, role_command};
use sc_error::Result;

/// The stack each tokio worker thread gets: 8 MiB rather than tokio's 2 MiB.
///
/// A trigger cascade nests on the stack of the task that started it, and the
/// cascade bound (`sc_action::MAX_DEPTH`, 10) lets a trigger that starts an
/// agent whose tool runs that trigger again nest ten agent runs deep before it
/// is refused. That loop is the deepest nesting there is, and in a debug build
/// it needs more than 2 MiB. Overflowing aborts the whole process, so the
/// margin is kept generous. Only the pages a thread touches are committed, so
/// the cost is address space.
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(main_async())
}

async fn main_async() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The **chain**, not just the outermost context: every boot step
            // wraps its failure in a sentence saying what it was doing, and
            // printing only that one ("ensuring the triggers table exists")
            // throws away the sentence that says what actually went wrong.
            eprintln!("error: {}", sc_error::format_chain(&e));
            ExitCode::FAILURE
        }
    }
}

/// Dispatch a subcommand.
async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("serve") => serve_command(&args[1..]).await,
        Some("build-app") => build_app_command(&args[1..]).await,
        Some("api") => api_command(&args[1..]).await,
        Some("get-cfg") => get_cfg_command(&args[1..]).await,
        Some("migrate") => migrate_command(&args[1..]).await,
        Some("set-cfg") => set_cfg_command(&args[1..]).await,
        Some("auth") => auth_command(&args[1..]).await,
        Some("mcp-token") => mcp_token_command(&args[1..]).await,
        Some("build-agent") => build_agent_command(&args[1..]).await,
        Some("add-user") => add_user_command(&args[1..]).await,
        Some("modify-user") => modify_user_command(&args[1..]).await,
        Some("delete-user") => delete_user_command(&args[1..]).await,
        Some("role") => role_command(&args[1..]).await,
        Some("backup") => backup_command(&args[1..]).await,
        Some("restore") => restore_command(&args[1..]).await,
        Some("app") => app_command(&args[1..]).await,
        Some("check") => check_command(&args[1..]).await,
        Some("module") => module_command(&args[1..]).await,
        Some("send-email") => send_email_command(&args[1..]).await,
        Some("agent") => agent_command(&args[1..]).await,
        Some("i18n") => i18n_command(&args[1..]).await,
        Some("cmdstan") => cmdstan_command(&args[1..]).await,
        Some("python") => python_command(&args[1..]).await,
        Some("demo") => demo_command(&args[1..]).await,
        // The profile is the file the binary was built with, so the one a
        // container is run under always matches the image it runs.
        // The build's version, commit and build time — the System tab's line.
        Some("--version" | "-V" | "version") => {
            println!("{}", sc_server::version_line());
            Ok(())
        }
        Some("seccomp-profile") => {
            print!("{}", sc_cli::SECCOMP_PROFILE);
            Ok(())
        }
        // Synchronous: it runs package managers and writes files, and touches
        // no database.
        Some("setup") => sc_cli::setup::run_setup(&args[1..]),
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown command `{other}`"
        ))),
        None => {
            print_usage();
            Ok(())
        }
    }
}

/// Print the short usage summary.
#[allow(clippy::too_many_lines)]
fn print_usage() {
    eprintln!("feldspar — usage:");
    eprintln!("  feldspar --version   the version, git commit and build time of this binary");
    eprintln!("  feldspar setup [--server | --dev] [--help for the rest]");
    eprintln!("  feldspar serve [database flags] [server flags]");
    eprintln!("  feldspar seccomp-profile   the seccomp profile for running the Docker image");
    eprintln!("  feldspar build-app SUBDOMAIN [database flags] [--file-store NAME=PATH]");
    eprintln!("  feldspar api add-query --app SUBDOMAIN [--api MOUNT] --name NAME");
    eprintln!(
        "                        [--method GET] --path /sub/path [--min-role N] \
         [--description TEXT]"
    );
    eprintln!("                        [--param name:type[,name:type…]]…");
    eprintln!(
        "                        [--language sql|javascript|python] --code TEXT|@FILE \
         (or --sql)"
    );
    eprintln!("  feldspar api list-queries --app SUBDOMAIN [--api MOUNT]");
    eprintln!("  feldspar api remove-query --app SUBDOMAIN [--api MOUNT] --name NAME");
    eprintln!("  feldspar check run [--app SUBDOMAIN] [--checklist NAME] [database flags]");
    eprintln!("  feldspar check list [--app SUBDOMAIN] [--checklist NAME] [database flags]");
    eprintln!("  feldspar migrate [status] [database flags]");
    eprintln!("  feldspar get-cfg [KEY] [--json] [database flags]");
    eprintln!("  feldspar set-cfg KEY [VALUE] [database flags]   (no VALUE: read it from stdin)");
    eprintln!(
        "  feldspar set-cfg --json [database flags] < settings.json   (several keys, one transaction)"
    );
    eprintln!("  feldspar set-cfg --unset KEY [database flags]");
    eprintln!("  feldspar agent eval SUITE [--model provider/model] [--strong P/M] [--cheap P/M]");
    eprintln!("                            [--task NAME]… [--out DIR] [--keep] [database flags]");
    eprintln!(
        "  feldspar i18n extract|lint|check [--domain core|admin|builder|analytics] [--locale TAG]"
    );
    eprintln!("  feldspar i18n lint PATH…");
    eprintln!(
        "  feldspar i18n translate --domain NAME --locale TAG [--provider NAME] [--model NAME]"
    );
    eprintln!("  feldspar i18n translate --instance --locale TAG [--provider NAME] [--model NAME]");
    eprintln!("                          [database flags]");
    eprintln!("  feldspar auth token --app SUBDOMAIN (--email EMAIL | --admin | --role NAME)");
    eprintln!("                      [--format playwright|netscape] [--out PATH] [--url ORIGIN]");
    eprintln!(
        "  feldspar mcp-token create --label TEXT (--email EMAIL | --admin) [--expires-in-days N]"
    );
    eprintln!("                      [--allow-all]");
    eprintln!(
        "                      [--allow-create|--no-allow-create] [--allow-edit|--no-allow-edit]"
    );
    eprintln!("                      [--allow-drop|--no-allow-drop]");
    eprintln!("                      [--checklists none|append|full]");
    eprintln!("                      [--allow-access-changes|--no-allow-access-changes]");
    eprintln!("                      [--allow-triggers|--no-allow-triggers]");
    eprintln!("                      [--allow-applications|--no-allow-applications]");
    eprintln!("                      [--allow-modules|--no-allow-modules]");
    eprintln!("                      [--url ORIGIN] [--name NAME] [database flags]");
    eprintln!(
        "  feldspar mcp-token create --label TEXT --email EMAIL --routes-only [--expires-in-days N]"
    );
    eprintln!("                      (module routes only, as a user of any role)");
    eprintln!("  feldspar mcp-token list [--json] [database flags]");
    eprintln!("  feldspar build-agent --server URL [--token-file PATH] [--workspace DIR]");
    eprintln!("                      [--insecure-http] [--once]");
    eprintln!("                      (or the token in FELDSPAR_BUILD_TOKEN)");
    eprintln!("  feldspar mcp-token revoke ID [database flags]");
    eprintln!(
        "  feldspar add-user EMAIL --role ROLE [--password [VALUE] | --random-password] \
         [database flags]"
    );
    eprintln!(
        "  feldspar modify-user EMAIL [--role ROLE] [--password [VALUE] | --random-password] \
         [--disabled|--enabled] \
         [--reset-second-factor] [database flags]"
    );
    eprintln!("  feldspar delete-user EMAIL [database flags]");
    eprintln!("  feldspar role list [--json] [database flags]");
    eprintln!("  feldspar role add NAME --level N [--description TEXT] [database flags]");
    eprintln!("  feldspar role delete NAME [database flags]");
    eprintln!("  feldspar app list [--json] [database flags]");
    eprintln!(
        "  feldspar app triggers (add|remove|list) --app SUBDOMAIN [NAME...] [--json] [database flags]"
    );
    eprintln!(
        "  feldspar app tables (add|remove|list) --app SUBDOMAIN [TABLE...] [--json] [database flags]"
    );
    eprintln!(
        "  feldspar app api-tables (add|remove|list) --app SUBDOMAIN [TABLE...] [--json] [database flags]"
    );
    eprintln!(
        "  feldspar module install --local DIR [--python] [--grant-read PATH]... [--grant-write PATH]..."
    );
    eprintln!(
        "                          [--grant-net HOST]... [--grant-env VAR]... [--setting KEY=VALUE]..."
    );
    eprintln!("                          [--force] [--json] [--modules-dir DIR] [database flags]");
    eprintln!(
        "  feldspar module grant NAME (--read|--write|--net|--env) VALUE [--revoke] [database flags]"
    );
    eprintln!("  feldspar module configure NAME KEY=VALUE... [database flags]");
    eprintln!("  feldspar module list [--json] [database flags]");
    eprintln!("  feldspar module remove NAME [database flags]");
    eprintln!("  feldspar send-email --to ADDR --subject TEXT [database flags] < body");
    eprintln!("  feldspar backup FILE|- [--password [P]] [database flags]");
    eprintln!("  feldspar restore FILE [--password [P]] [database flags] [server flags]");
    eprintln!("  feldspar demo analytics [--replace] [database flags]");
    eprintln!("  feldspar cmdstan status [--cmdstan DIR]");
    eprintln!("  feldspar cmdstan install [--version V] [--dir D] [--jobs J]");
    eprintln!(
        "  feldspar python check [--url URL | --offline] [--python-dir PATH] [--python-bin PATH] \
         [--import MODULE]..."
    );
    eprintln!();
    eprintln!("  database (or the DATABASE_URL / PG* environment variables):");
    eprintln!("    --database-url URL   full connection string (takes precedence)");
    eprintln!("    --db-host H  --db-port N  --db-user U  --db-password P  --db-name D");
    eprintln!(
        "    --sqlite PATH        use a SQLite file as the primary database instead \
         (or FELDSPAR_SQLITE);"
    );
    eprintln!("                         it is created if it is not there");
    eprintln!();
    eprintln!("  configuration file (used for whatever the flags and environment leave unset):");
    eprintln!(
        "    --environment NAME   which [environments.NAME] section to connect with \
         (or FELDSPAR_ENV);"
    );
    eprintln!("                         naming one makes it outrank DATABASE_URL / PG*");
    eprintln!("    --config PATH        read this file instead of searching (or FELDSPAR_CONFIG)");
    for path in sc_config_file::search_paths() {
        eprintln!("                         searched: {}", path.display());
    }
    eprintln!();
    eprintln!(
        "  setup: makes this machine an instance — development (you run `feldspar serve`;
       the configuration in your own config directory) or production (a
       `feldspar` system account and a systemd or OpenRC service; the
       configuration in /etc/feldspar) — over SQLite or PostgreSQL, local
       (installed if need be) or remote. --server is production with a local
       PostgreSQL and --dev is development with SQLite, both asking nothing;
       otherwise what the flags leave open is asked. --dry-run prints the plan.

  build-app: builds one application and prints the bundler's output.

  api: adds, lists and removes an application's custom SQL queries. A query is
       validated by preparing it, so one that will not prepare is refused with
       the database's own message and nothing is stored; adding or removing one
       rewrites the application's generated client. A parameter is written
       `name:type`, or `name:type?` when the caller may leave it out. Every
       query has a minimum role, and it is **admin** unless --min-role says
       otherwise: raw SQL does not go through the row layer, so ownership
       formulae do not filter what it returns.

  get-cfg / set-cfg: the settings an admin edits in Settings, from a terminal.
       They are rows in the primary database, so these commands need the database
       and nothing else — no running server, no session. `get-cfg KEY` prints that
       value alone, ready to capture (`port=$(feldspar get-cfg smtp_port)`);
       `get-cfg` with no key prints every declared setting as `key=value`, one per
       line, with secrets shown as the redaction the admin UI shows — name a
       secret's key to see it in full. `set-cfg` takes the value from the command
       line, or from stdin when there is none, which is how a multi-line PEM block
       is set: `feldspar set-cfg ssl_certificate < fullchain.pem`. The value is
       checked against the key's declared type before it is written, so
       `set-cfg smtp_port yes` is a message rather than a stored string. Nothing
       is restarted: when a setting takes effect is the setting's own business.

  auth token: mints a session on the *running* server and writes the cookies a
       browser would have got, so a script can screenshot the screens behind the
       sign-in page. It asks no password: this command already holds the
       database, which is more authority than any password buys. Say who the
       session is for with --email EMAIL, with --admin (the first admin user) or
       with --role NAME (the first user holding that role — the error lists the
       roles when the name is not one). It forges nothing: the session is minted
       by the server, from a one-time grant, and can do exactly what that account
       can. The default file is .feldspar-session.json, Playwright's
       storageState; --format netscape writes a cookies.txt for curl instead.
       Both are written 0600 — a session file is a password.

  mcp-token: mints, lists and revokes the bearer tokens the administration MCP
       server (POST /mcp) accepts — the same tokens Settings → Development
       mints. A token runs as an administrator: name one with --email, or
       --admin for the first. Each of the seven grants has a flag and its --no-
       form; one left out is granted. --allow-all also grants every grant a
       later version adds, stored on the token, and --no-allow-… still takes
       single ones out (--allow-all --no-allow-drop). The secret is printed
       once, alone on stdout; the id to revoke it by and the `claude mcp add`
       line go to stderr. Turn the server on with `set-cfg mcp_enabled true`.

  add-user / modify-user: create an account, or change one's role or password,
       straight in the database — the way back in for an admin who has forgotten
       their password on a server with no SMTP to send a reset link. ROLE is a
       role name (admin) or number (1). A password is never needed on the command
       line: leave --password out of add-user, or give it no value, and it is
       asked for without echo. Without a terminal, --password with no value reads
       one line of stdin, and add-user with no --password is refused — stdin is
       never read unless --password asks for it, so a `while read` loop over a
       file cannot hand its next line in as a password. --random-password
       generates one and prints it once on stdout. A password starting with -- is
       written --password=VALUE.
       modify-user --disabled stops an account signing in (and ends its
       sessions) without deleting it; --enabled gives it back.
       modify-user --reset-second-factor turns off the account's two-factor
       authentication (for a lost phone and lost recovery codes) and ends its
       sessions.

  delete-user: deletes an account. The last enabled admin cannot be deleted
       or disabled, here or anywhere else.

  role: lists, adds and deletes roles. --level is the role's number, 1 (admin)
       to 100 (public); a lower number is more privileged. A built-in role, or
       one a user still holds, cannot be deleted.

  backup: writes everything this installation has to FILE, as the zip
       Settings → Backup downloads with every box ticked but SSL / TLS: the
       certificate, its key and the names it covers belong to this machine.
       The file's directory must exist; a FILE already there is replaced only
       once the new one is complete. A FILE of - writes the zip to stdout
       instead, with no temporary file (feldspar backup - | aws s3 cp - …).
       --password P, or the FELDSPAR_BACKUP_PASSWORD environment variable,
       encrypts every entry with AES-256 (WinZip AES), which 7-Zip and other
       standard tools open with the same password; --password with no value
       asks for it. A password on the command line is visible to other users
       of the machine (ps) — prefer the prompt or the variable.

  restore: restores everything in FILE — a backup from `feldspar backup`, from
       Settings → Backup, or from Saltcorn 1 — exactly as the Restore dialog
       does with nothing unticked. A restore adds and does not destroy: it
       drops no table, deletes no row and replaces no account, and each thing
       it left alone or could not restore is printed with why. It assembles what
       `serve` does to do it (modules are reinstalled, applications built), so
       give it the server flags the server is started with, or the same
       --environment. A server already running on the database loads the
       result when it is restarted. A password-protected backup is opened
       with --password P, or FELDSPAR_BACKUP_PASSWORD, or the password asked
       for at the terminal.

  app list: every application with its framework and the project directory
       its source is in on this machine; --json prints an array of
       {{id, name, subdomain, framework, file_store, source_dir, project_dir}}.

  app triggers: the triggers an application exposes, each as
       POST <api mount>/actions/<name> and api.run<Name>() in its generated
       client. add and remove change the list and rewrite the client; only a
       trigger with no event (when none) can be added. --json prints
       {{name, action, min_role, method, route, note}} per trigger. A server
       already running on the database serves the change when it is restarted;
       update_application over MCP changes a running one at once.

  server:"
    );
    eprintln!("    --bind ADDR  --static-dir DIR  --session-ttl-hours N  --secure-cookies");
    eprintln!(
        "    --https-port N           the port HTTPS is served on when TLS is on (default 443)"
    );
    eprintln!("    --base-domain DOMAIN     apps are served at <subdomain>.<domain>");
    eprintln!(
        "    --extra-base-domain D    …and also at <subdomain>.<D> (repeatable), e.g. 10.0.2.2.nip.io
                             for an Android emulator"
    );
    eprintln!(
        "    --browser PATH           the headless Chromium view_app drives (default: chromium,
                             chromium-browser or google-chrome on PATH, not a snap)
    --no-browser-sandbox     start that browser with --no-sandbox
    --browser-contexts N     view_app runs that may hold a browser context at once (default 4)
    --language-server PATH   the typescript-language-server the IDE runs (default: on PATH)
    --claude-cli PATH        the Claude Code the IDE's terminal runs (default: claude on PATH)
    --preview-idle-minutes N unmount a run's preview after N idle minutes (default 60)"
    );
    eprintln!(
        "    --max-request-body N     the largest request body read into memory, in bytes or
                             with K/M/G (default 2M); bigger bodies are a 413
    --max-upload-bytes N     the largest file one upload may be: the file manager, a
                             backup, an application's upload slots (default 256M)
    --code-max-file-bytes N  the most one code-body file read or write may carry
                             (default 8M); larger files are read in pieces
    --code-max-copy-bytes N  the most one host-side copy or concat in a code body
                             may move (default 256M)
    --trusted-proxies LIST   proxies (addresses or CIDR blocks, comma-separated) whose
                             X-Forwarded-For names the client, for per-IP rate limits"
    );
    eprintln!(
        "    --file-store NAME=PATH   connect a local directory as a named file store (repeatable)"
    );
    eprintln!(
        "    --code-workers N         V8 isolates serving run_js_code bodies (default 2)
    --code-max-inflight N    runs each of those isolates keeps resident (default 256)
    --modules-dir PATH       where modules are installed (default: the platform's
                             data directory, e.g. ~/.local/share/feldspar/modules)"
    );
    eprintln!(
        "    --python auto|off        whether this process starts its Python interpreter
                             (default auto: started by the first Python body, and
                             never in a binary built without the `python` feature)
    --python-max-inflight N  Python runs resident at once (default 32)
    --python-max-stuck N     runs that never returned before Python is refused
                             until a restart (default 8)
    --python-dir PATH        the virtual environment Python modules install into
                             (default: the platform's data directory, e.g.
                             ~/.local/share/feldspar/python)
    --python-bin PATH        the interpreter pip runs under (default: the release's
                             bundled python/bin/python3 beside bin/feldspar, else
                             python3 on PATH). It must be the same major.minor
                             version as the one this server was built against, or
                             the environment is refused rather than segfaulted on"
    );
    eprintln!(
        "    --model-max-rows N       rows one model dataset may select (default 200000)
    --cmdstan DIR            the CmdStan Stan models use (default: $CMDSTAN, else
                             the newest ~/.cmdstan/cmdstan-*)
    --stan-cache-dir DIR     where compiled Stan programs are kept (default: the
                             platform's data directory, e.g.
                             ~/.local/share/feldspar/stan-cache)
    --stan-max-processes N   Stan chain processes at once, across every fit
                             (default: half the CPUs, at least 1)
    --stan-max-data-values N numbers one fit's bound data may hold (default 20000000)
    --stan-max-draws-bytes N bytes of draws one fit may store (default 1000000000)
    --stan-max-draws-response N
                             numbers one draws response may carry (default 2000000)
    --stan-summary-max-elements N
                             a generated quantity larger than this is summarised
                             on demand rather than at fit time (default 1000)"
    );
    eprintln!();
    eprintln!(
        "  a feldspar.toml environment may also carry `base_domain`, `extra_base_domains`, `bind`,
  `secure_cookies`, `https_port`, `browser`, `browser_sandbox`, `cmdstan` and the `stan_*` keys, so `serve --environment NAME` needs none of those flags —
  and so a build from the command line writes the same application URL into the
  generated documentation that the server would."
    );
    eprintln!();
    eprintln!(
        "  it may also pin the TLS settings — `ssl_mode`, `acme_contact_email`,
  `acme_directory_url`, `redirect_http_to_https` and `ssl_extra_domains` (a list). A key
  given there wins over the stored setting: Settings → SSL / TLS shows it read-only, and
  neither a restore nor Clear all can change it."
    );
}
