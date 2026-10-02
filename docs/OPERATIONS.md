# Operations manual

Running a Saltcorn Feldspar installation: getting a binary onto a host, keeping it
current, telling it which database to use, reloading it without a restart, and
letting a coding agent administer it.

This is the operator's document. [`README.md`](../README.md) is the reference for
every flag and every setting and stays authoritative where the two overlap; this
one is the order things are done in.

> **The system is a prototype.** It does not migrate databases created by older
> builds. Every upgrade path below begins with a dump, and that is not a
> formality.

Contents:

1. [The two artifacts](#1-the-two-artifacts)
2. [Installing](#2-installing)
3. [Updating](#3-updating)
4. [The configuration file](#4-the-configuration-file)
5. [Environment variables](#5-environment-variables)
6. [Reloading without a restart](#6-reloading-without-a-restart)
7. [Claude Code over the MCP server](#7-claude-code-over-the-mcp-server)
8. [Day-to-day operations](#8-day-to-day-operations)
9. [Bayesian models with Stan](#9-bayesian-models-with-stan)

---

## 1. The two artifacts

There is one binary, `feldspar`, and two ways to get it. Everything else in this
document is the same for both.

| | **Static artifact** | **From source** |
|---|---|---|
| Where it comes from | `https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz`, or `scripts/build-static.sh` in a checkout | `cargo build --release -p sc-cli` |
| Linkage | statically linked glibc (`+crt-static`), a static-PIE: no shared libraries, no interpreter | dynamic, against this machine's glibc |
| Runs on | Debian, Ubuntu, RHEL **and** Alpine, from the same tarball | the machine it was built on, and machines like it |
| Toolchain on the host | none | Rust, a C toolchain, `libclang`, Node, a few GB of RAM |
| Installed at | `/opt/feldspar` (the prefix is compiled in) | `/usr/local/bin/feldspar`, with the checkout kept where it was built |
| **Python triggers** | **no** — embedding CPython means linking `libpython`, which a static binary cannot do | yes, with `--features python` |
| Native Node addons in modules | no — a static binary cannot `dlopen` | yes |
| **Built-in model providers** | yes — five smartcore providers plus two hypothesis tests | the same, unless built `--no-default-features` |
| Name resolution | in-process (`sc-dns`), **not** glibc NSS | glibc |

Two consequences of the static build are worth knowing before you choose it:

- **A Python-capable server is the from-source build.** There is no flag that adds
  Python to a binary that was not built with `--features python`, and the shipped
  tarball is not one. A trigger with a Python body on such a server fails at fire
  time with a message saying so, rather than at configuration time — so a
  trigger's configuration keeps its meaning across deployments.
- **The machine-learning built-ins are a default-on cargo feature.** `sc-model`'s
  `smartcore` feature carries `linear_regression`, `logistic_regression`,
  `random_forest`, `kmeans` and `pca`; a build with `--no-default-features` leaves
  only `t_test` and `anova`, which need a distribution function and nothing else. That
  is a supported build, not a broken one — the Analytics UI's model list says on the screen that the
  machine-learning built-ins were compiled out, and a module can still supply
  providers. Like `--features python`, it is decided at build time and no run-time flag
  substitutes for it.
- **`/etc/nsswitch.conf` does not apply to the static binary.** It resolves names
  itself, reading `/etc/resolv.conf` and `/etc/hosts` and nothing else. mDNS
  (`.local`), `myhostname`'s synthesis of the local hostname, and LDAP/sssd hosts
  are invisible to it. A name it must reach — your database host, your SMTP
  relay, the ACME CA — has to be in DNS or in `/etc/hosts`.

Both artifacts carry the admin SPA, the file-store IDE, **Saltcorn UI's view
runtime** (`ui/saltcorn-ui/dist`: Saltcorn 1's view code, bundled, and the browser
assets its HTML loads) and **its layout builder** (`ui/builder/dist`: Saltcorn 1's
drag-and-drop builder, CKEditor 4 and Monaco) — four bundles, built together or not at
all (§5.3). A
Saltcorn UI application needs nothing on the host beyond that directory: no build,
no npm, no `node`. Both artifacts need `npm` on
the host *at run time* if applications will be built or modules installed there:
the server shells out to npm for those, from the admin UI's **Build** button as
much as from `feldspar build-app`. It never runs `node` — a module runs on a
JavaScript worker inside the `feldspar` process, on the V8 the server already
links.

That npm has to be **9.3.0 or newer** to install a module. Debian 12, Debian 13
and Ubuntu 24.04 all package npm 9.2.0, which cannot: the modules directory
depends on the v1 API stub packages at a `file:` path and overrides the same
names, and npm before 9.3.0 (arborist 6.1.6) hands the path to semver, so every
install fails with `Invalid comparator: file:/…/v1-api-stub/saltcorn-data`
whatever is being installed. The server checks before it runs npm and says so;
`scripts/setup-host.sh` installs Node from NodeSource for this reason, and
`sudo npm install -g npm@latest` fixes a host that already has the old one.

---

## 2. Installing

Both modes end at the same place: a `feldspar` binary, a PostgreSQL role and
database, `/etc/feldspar/feldspar.toml`, a `feldspar` service account and a
systemd unit. `scripts/setup-host.sh` is what produces the last four, and it is
the same script in both modes — it only differs in whether it builds a binary or
expects to find one.

Everything below assumes a `sudo`-capable login on Debian or Ubuntu, and uses
`example.com` as the domain applications will be served under.

### 2.1 Mode A — the static artifact, downloaded

The short path. No checkout, no compiler, six commands:

```bash
curl -fLO https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz
curl -fLO https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz.sha256
sha256sum -c feldspar.tar.gz.sha256                     # optional, and quick
tar -xzf feldspar.tar.gz
sudo feldspar-*/install.sh                              # the tree at /opt/feldspar
sudo /opt/feldspar/setup-host.sh --domain example.com   # packages, database, unit
```

Those URLs **carry no version**. The objects behind them are always the latest
build, which is also what makes them the upgrade path (§3.2); there is no version
to look up and no release page to read. The published `.sha256` names
`feldspar.tar.gz` rather than the versioned file it was computed over, so
`sha256sum -c` works on exactly what you downloaded.

`install.sh` copies the tree to `/opt/feldspar` and puts a copy of `setup-host.sh`
beside it, which is why the third command is run from there rather than fetched
again. Run out of an installed tree like that, the script sees the binary beside
it and takes `--static` as its default — there is nothing to build and no flag to
remember.

What lands where:

```
/opt/feldspar/bin/feldspar        the binary (symlinked to /usr/local/bin/feldspar)
/opt/feldspar/ui/admin/dist       the admin SPA it serves
/opt/feldspar/ui/ide/dist         the file-store IDE it serves
/opt/feldspar/plugins/            the modules it ships with, one click to install
/opt/feldspar/install.sh          copies the tree into place
/opt/feldspar/setup-host.sh       the host setup, run once
```

**The prefix is not cosmetic.** `crates/sc-cli/build.rs` compiles the two bundle
paths into the binary, and the IDE's has no run-time flag to override it. To
install anywhere but `/opt/feldspar`, rebuild with `scripts/build-static.sh
--prefix <path>` — an artifact built for one prefix and installed at another
serves a blank page.

### 2.2 Mode B — compiled from source, on the host

The host does the building. One command, and it wants a C toolchain, `libclang`,
Node and a few GB of RAM:

```bash
curl -fsSL https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \
  | sh -s -- --domain example.com
```

That installs rustup as the invoking user, clones the repository to
`/opt/feldspar/src`, builds `cargo build --release -p sc-cli`, and installs the
result at `/usr/local/bin/feldspar`. **The checkout has to stay where it was
built**: the release build also runs `npm ci && npm run build` in `ui/admin` and
`ui/ide` and records their absolute paths in the binary.

Piping a script off the internet into a root shell deserves one look first.
`--dry-run` prints every command it would run and every file it would write,
and changes nothing:

```bash
wget -qO- https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \
  | sh -s -- --dry-run --domain example.com
```

Doing it by hand instead is README §2.1–§2.6: packages, a service account, the
role and database, the build, `feldspar.toml`, the unit. Add `--features python`
to the cargo line if this server is to run Python trigger bodies or install Python
modules (§1) — it is a build-time decision and the only one.

### 2.3 Mode C — built on one machine, pushed to another

Build the static artifact in a checkout and install it over ssh in one command.
This is the mode for a fleet: one machine has the toolchain, the rest have the
binary.

```bash
scripts/build-static.sh                          # dist/feldspar-<version>-<target>.tar.gz
scripts/build-static.sh --deploy root@vm         # ...and install it there
scripts/build-static.sh --deploy vm --ssh-opt -p2222
```

Then, on a host that has never run feldspar, the other half of the two-step
install:

```bash
ssh -t vm 'sudo /opt/feldspar/setup-host.sh --domain example.com'
```

**Either order works.** With `--static`, `setup-host.sh` never starts a server
whose binary is not there yet: it enables the unit and leaves it stopped, and an
earlier setup is finished by `systemctl start feldspar`.

What `--deploy` does on the remote, in order: copies the tarball to `/tmp`
(`--remote-tmp` elsewhere), unpacks it, **stops `feldspar.service` if and only if
it is running**, runs `install.sh` under `sudo` unless the login is root, starts
the unit again if it stopped it, runs the installed binary once as a smoke check,
and removes the staging copy. A machine with no systemd, no `feldspar.service`, or
a service deliberately left down is left exactly as it was — nothing is started
behind your back. That the host answers ssh at all is checked **before** the build
starts rather than after it.

Build-environment options worth knowing:

| Flag | Effect |
|---|---|
| `--docker` | build in a container with a pinned Rust and Node. The default when docker with buildx is present, because it does not depend on what this machine has installed |
| `--native` | build with this machine's toolchain. Needs the target added to rustup, clang/libclang, cmake, a C compiler, `libz.a` (`zlib1g-dev`) and, unless `--no-ui`, node and npm |
| `--target aarch64-unknown-linux-gnu` | the other supported target. The **musl** targets are refused by name: V8 reaches this build as `rusty_v8`'s prebuilt static archive, which upstream publishes for gnu, darwin and Windows only |
| `--prefix PATH` | the absolute directory the artifact will be installed to, compiled into the binary |
| `--no-ui` | skip the five front-end bundles (`SC_BUILD_ADMIN=0`), so no Node toolchain is needed — and the artifact has no admin UI, no IDE, **no Saltcorn UI**, no builder and no Analytics UI (§5.3) |
| `-j N` | lower cargo's parallelism if the linker runs the machine out of memory. This workspace links V8 |
| `--no-verify` | skip the post-build checks — static linkage, and a smoke run in Debian and Alpine containers |

### 2.4 What `setup-host.sh` writes

Worth reading once, because these are the files you will edit later.

`/etc/feldspar/feldspar.toml`, mode `0600`, owned by the service account — the
file may hold a database password, and the server warns on stderr when other
users can read it:

```toml
default_environment = "production"

[environments.production]
host = "/var/run/postgresql"   # a leading `/` is a Unix socket directory
user = "feldspar"
database = "feldspar"

base_domain = "example.com"
bind = "0.0.0.0:80"
```

The local database uses **peer authentication over the Unix socket** — the role
has the name of the system account, so the service connects as itself and no
database password is written to disk anywhere. `--database-url` instead points at
an existing PostgreSQL elsewhere, and then nothing is installed or created here.

`/etc/systemd/system/feldspar.service`:

```ini
[Service]
Type=notify
WatchdogSec=30s
User=feldspar
ExecStart=/opt/feldspar/bin/feldspar serve --environment production
Environment=FELDSPAR_CONFIG=/etc/feldspar/feldspar.toml
Environment=HOME=/var/lib/feldspar
Environment=SC_DATA_DIR=/var/lib/feldspar
StateDirectory=feldspar
WorkingDirectory=/var/lib/feldspar
AmbientCapabilities=CAP_NET_BIND_SERVICE
ProtectSystem=strict
ReadWritePaths=/var/lib/feldspar
```

Five decisions in there:

- **`Type=notify`**, so `systemctl start` returns when the port is accepting
  rather than when the process exists. A unit ordered `After=feldspar.service`
  then does not race the listener, and a boot that dies while connecting to the
  database does not look like a successful start. The process also sends
  `EXTEND_TIMEOUT_USEC` while a boot step that can legitimately take minutes runs
  — building an application runs `npm install` — so `TimeoutStartSec` does not
  have to be sized for the worst case.
- **`--environment production` is named outright**, even though it is the file's
  default, so an ambient `DATABASE_URL` cannot redirect the service (§4.3).
- **`AmbientCapabilities=CAP_NET_BIND_SERVICE`** binds 80 and 443 without root.
- **`SC_DATA_DIR=/var/lib/feldspar`** is the one writable path under
  `ProtectSystem=strict`, and naming it outright is what lets the admin UI
  *suggest* a directory for a new file store. Without it the server would derive
  one from `HOME` — writable, but a place no operator would think to look.
- **A file that already exists is kept.** Re-running the script does not overwrite
  an edited unit or a config with a password in it; `--force` does.

### 2.5 The headless browser

The in-server coding agent looks at the application it has just built with its
`view_app` tool: a headless Chromium that the **server starts itself, as the
service account**, and drives over the DevTools protocol (TODO §7b). So setup
installs one that works there:

| Distribution | What is installed |
|---|---|
| Debian 12, 13 | apt's `chromium` |
| Ubuntu 24.04+ (amd64) | `google-chrome-stable`, from Google's apt repository |
| Ubuntu on another architecture | nothing: the step says so, and `view_app` is unavailable |

**Not Ubuntu's `chromium-browser`.** That apt package is a transitional one: its
`/usr/bin/chromium-browser` is a shell script that execs the Chromium *snap*, and
a snap does not start under a systemd service user with `ProtectHome` and
`PrivateTmp`. The server skips a snap shim when it searches `PATH` for the same
reason, so having one installed does no harm, but it does not count.

A browser already on `PATH` that is not a snap shim is kept. Setup then starts it
once as the service account, the way the server will:

```sh
runuser -u feldspar -- google-chrome --headless --user-data-dir=/tmp/… --dump-dom about:blank
```

and reports the browser's own error if that fails. If it only works with
`--no-sandbox` (a kernel that refuses unprivileged user namespaces), setup says so,
and `browser_sandbox = false` in the environment (§4.2) lets the server do the
same. `--no-browser` skips the whole step.

The server finds the browser by the `browser` key in the configuration file, or
on `PATH` as `chromium`, `chromium-browser` or `google-chrome`, and says at
startup which one it found — or that `view_app` is unavailable and why:

```
feldspar: view_app will use the browser at /usr/bin/google-chrome
feldspar: view_app is unavailable: no browser found (…); install Chromium or set `browser` in feldspar.toml
```

### 2.6 What setup does not do

Four things, and three of them happen in a browser (README §2.7):

1. **DNS.** Point `example.com` at the host, and a wildcard `*.example.com`
   beside it — each application is a subdomain of the base domain.
2. **The first admin user.** Open `http://example.com` and the SPA offers a
   create-first-user screen. **That screen is open to whoever reaches it first**,
   so do it before the host is reachable from the internet, or do it over an ssh
   tunnel.
3. **TLS**, in the admin UI under **Settings → SSL / TLS certificates** — not a
   command-line flag. Choose `letsencrypt`, give a contact address, and restart.
   Validation is TLS-ALPN-01, so port 443 must be reachable from the internet,
   and the certificate covers the base domain plus every mounted application's
   subdomain — **adding an application adds a name at the next restart**. Try the
   staging directory (`https://acme-staging-v02.api.letsencrypt.org/directory`)
   while you iterate: its certificates are untrusted, its rate limits are not.
4. **The firewall.** Only 80 and 443 need to be open. Postgres stays on its
   socket, and nothing else listens.

---

## 3. Updating

### 3.1 First, a dump

The prototype does not migrate databases created by older builds. Take the dump
every time, and check that it is not empty:

```bash
sudo -u feldspar pg_dump -h /var/run/postgresql feldspar > ~/feldspar-$(date +%F).sql
ls -l ~/feldspar-$(date +%F).sql
```

### 3.2 A static install — redownload

The same version-less URL that installed it. Only the binary and the bundles
change: `setup-host.sh` is **not** run again, and the database, `feldspar.toml`
and the unit are left alone.

```bash
curl -fLO https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz
curl -fLO https://feldspar-latest-static.saltcorn.com/feldspar.tar.gz.sha256
sha256sum -c feldspar.tar.gz.sha256
tar -xzf feldspar.tar.gz
sudo systemctl stop feldspar
sudo feldspar-*/install.sh
sudo systemctl start feldspar
```

**Stop the service first.** `install.sh` copies over `/opt/feldspar/bin/feldspar`,
and the kernel refuses to write the executable of a live process — that is what
`ETXTBSY` / `Text file busy` means if you try it while the server is up. Nothing
restarts by itself.

### 3.3 A source install — recompile

```bash
cd /opt/feldspar/src
git pull
cargo build --release -p sc-cli          # add --features python if it had it
sudo install -m 0755 target/release/feldspar /usr/local/bin/feldspar
sudo systemctl restart feldspar
```

`install` writes a new file rather than into the running one, so the running
server is undisturbed until the restart. Keep `--features python` on the rebuild
if the previous binary had it: it is a build-time decision, and a server that
quietly loses Python fails at the next Python trigger rather than at boot.

### 3.4 Recompile on one VM, `--deploy` to the rest

The fleet update. One machine holds the checkout and the toolchain; every other
host gets the artifact.

```bash
# On the build machine, in the checkout:
git pull
scripts/build-static.sh --deploy root@app1
scripts/build-static.sh --deploy root@app2
```

`--deploy` stops a running `feldspar.service` around the install and starts it
again, so **for a host that was already running, that one command is the whole
update** — there is no separate restart to remember. The build itself happens
once per invocation; to push one artifact to several hosts without rebuilding it,
build once and copy it by hand:

```bash
scripts/build-static.sh                       # dist/feldspar-<version>-<target>.tar.gz
for h in app1 app2 app3; do
  scp dist/feldspar-*-x86_64-unknown-linux-gnu.tar.gz "root@$h:/tmp/"
  ssh -t "root@$h" 'set -e
    tar -xzf /tmp/feldspar-*.tar.gz -C /tmp
    systemctl stop feldspar
    /tmp/feldspar-*/install.sh
    systemctl start feldspar
    rm -rf /tmp/feldspar-*'
done
```

**Publishing, rather than deploying.** `--release` uploads the tarball and its
`.sha256` to the Cloudflare R2 bucket `feldspar-latest-static` as
`feldspar.tar.gz` and `feldspar.tar.gz.sha256` — the object names carry no
version, so each release overwrites the last and the download URL of §2.1 never
changes:

```bash
scripts/build-static.sh --release
```

It runs `npx wrangler r2 object put --remote`, so it needs `npx` on `PATH` (which
is checked before the build starts) and a wrangler login with access to the
bucket. `--bucket NAME` uploads somewhere else; such a bucket is not served at
that hostname and is deliberately given no public URL in the output. The publish
ends by printing the six download commands, so what was uploaded and what
operators are told to fetch are visibly the same artifact.

### 3.5 What a restart costs

The restart rebuilds and remounts every stored application. One that fails to
build is logged and skipped rather than taking the server with it. To rebuild a
single application on its own — and to see the bundler's own diagnostics when it
fails:

```bash
feldspar build-app blog --environment production
```

That command **mounts nothing**, so it is safe to run against the live
deployment's database: no application is served by that process and it cannot
disturb what the server is serving.

Two things a restart also does, and a reload does not (§6): it **invalidates every
session**, because sessions are held in the server's memory, and it is when the
ACME order is built, so a newly added application gets its certificate name at
the next restart and not before.

**A model fit in flight does not survive it.** Fitting is a spawned job whose only
record is its `_fd_model_instances` row, so a process that stops mid-fit would leave a
row saying `fitting` for ever. Boot therefore **reaps** them: every instance still
`fitting` at startup is marked `failed` with *"the server restarted while this fit was
running"*. Nothing is lost but the compute — the model is untouched, and pressing
**Fit** again starts a new instance. An instance that was already `fitted` is
unaffected, including the **active** one that `predict("…")` in a formula reads, so
predictions resume with the restart.

---

## 4. The configuration file

### 4.1 Where it lives

`--config PATH` names it outright; `FELDSPAR_CONFIG` does the same from the
environment (which is what the systemd unit uses). Without either, it is searched
for, **user directory first**:

| | user | system |
|---|---|---|
| Linux/BSD | `$XDG_CONFIG_HOME/feldspar/feldspar.toml`, else `~/.config/feldspar/feldspar.toml` | `/etc/feldspar/feldspar.toml` |
| macOS | `~/Library/Application Support/feldspar/feldspar.toml` | `/etc/feldspar/feldspar.toml` |
| Windows | `%APPDATA%\feldspar\feldspar.toml` | `%PROGRAMDATA%\feldspar\feldspar.toml` |

The system path matters as much as the user one: a server started by systemd runs
as a service account that may have no home directory at all. `feldspar` with no
arguments prints the paths it would search on this machine.

**No file at all is fine** — that is the environment-variable deployment. But a
file that does not parse, a key that is not recognised, a `--config` path that
does not exist, and an `--environment` the file does not define are all startup
errors rather than things stepped over. A misspelled `databse` that was quietly
ignored would connect to the wrong database, which is worse than not starting.

Keep it to the server: `root:feldspar 0640` for the systemd deployment — the server
reads this file and never writes it, so leaving root the owner means a compromised
server cannot rewrite what the next restart connects to — or `chmod 600` when the file
is yours. It holds passwords, and the server warns on stderr when other users can read
it, counting a group that is not its own as other users.

### 4.2 The format

Two top-level keys, and one table of environments:

```toml
# Which environment to use when the command line selects none.
# Without this key and without --environment, the name used is "production".
default_environment = "production"

[environments.production]
# The database: either a URL, or the individual parts. A url wins over parts.
url = "postgres://feldspar:secret@db.internal:5432/feldspar"
# ...or:
#   host     = "db.internal"          # a leading `/` is a Unix socket directory
#   port     = 5432
#   user     = "feldspar"
#   password = "secret"
#   database = "feldspar"

# The serving half — these mirror `serve` flags exactly.
base_domain    = "example.com"        # apps are served at <subdomain>.example.com
bind           = "0.0.0.0:443"
secure_cookies = true
browser        = "/usr/bin/chromium"  # view_app's browser; unset searches PATH

# This host's CmdStan and the Stan ceilings (§9) — each mirrors a `serve` flag.
cmdstan            = "/var/lib/feldspar/.cmdstan/cmdstan-2.40.0"
stan_max_processes = 4

[environments.staging]
url = "postgres://feldspar:secret@staging.internal:5432/feldspar"
base_domain = "staging.example.com"

[environments.laptop]
# The other kind of database: a file, with no host, no role and nothing to start.
sqlite = "/home/me/feldspar/app.sqlite"

[environments.test]
database      = "feldspar_test"       # read by `cargo test`, not by the server
test_template = "sc_template"
```

Every key an environment may hold:

| Key | Type | Meaning |
|---|---|---|
| `url` | string | full PostgreSQL connection string. Wins over the individual parts |
| `host` | string | database host; a leading `/` is a Unix socket **directory** |
| `port` | integer | database port |
| `user` | string | role to connect as |
| `password` | string | its password. This is why the file is `0600` |
| `database` | string | database name |
| `sqlite` | string | a SQLite file instead of a Postgres server. Created if absent |
| `base_domain` | string | applications are served at `<subdomain>.<base_domain>` |
| `bind` | string | `address:port` to listen on |
| `secure_cookies` | boolean | set `Secure` on session and CSRF cookies |
| `browser` | string | the headless Chromium the coding agent's `view_app` drives. Unset: `chromium`, `chromium-browser` or `google-chrome` on `PATH`, skipping a snap (§2.5) |
| `browser_sandbox` | boolean | `false` starts that browser with `--no-sandbox`, for a kernel that refuses its sandbox. Default `true` |
| `cmdstan` | string | the CmdStan Stan models use — `--cmdstan` (§9.1) |
| `stan_cache_dir` | string | where compiled Stan programs are kept — `--stan-cache-dir` (§9.3) |
| `stan_max_processes` | integer | chain processes at once, across every fit — `--stan-max-processes` (§9.2) |
| `stan_max_data_values` | integer | `--stan-max-data-values` (§9.4) |
| `stan_max_draws_bytes` | integer | `--stan-max-draws-bytes` (§9.4) |
| `stan_max_draws_response` | integer | `--stan-max-draws-response` (§9.4) |
| `stan_summary_max_elements` | integer | `--stan-summary-max-elements` (§9.4) |
| `test_template` | string | the template per-test databases are cloned from. Read by the integration-test harness, **not** by the server |

`environments` is an ordinary TOML table: define as many as you have databases,
named whatever you like. `--environment NAME` (or `--env NAME`, or `FELDSPAR_ENV`)
picks one.

Three rules that are easy to get wrong:

- **`sqlite` is exclusive with the Postgres parameters.** A section naming both is
  refused rather than quietly preferring one. SQLite is a full backend — composite
  keys, foreign keys, `RETURNING`, transactions, the schema editor — with two
  genuine absences: row-level security (authorization is enforced above the
  database, and the tables list says `rls_available: false`) and `LISTEN`/`NOTIFY`
  (so the message bus does not use the database).
- **The serving keys are here so that a command-line build agrees with the
  server.** `feldspar build-app` run against the same environment writes the
  application's real URL into the documentation it generates, instead of a
  placeholder. An environment with connection parameters and no `base_domain`
  stops `feldspar auth token` with `no base domain`.
- **Unknown keys are errors** (`deny_unknown_fields`), at both levels.
- **A flag beats the file.** The serving and Stan keys are read as if they had been
  typed before the command line's own flags, so `--stan-max-processes 8` on one run
  overrides `stan_max_processes = 4`, and a `0` in the file is refused with the same
  sentence as a `0` on the command line.

### 4.3 Precedence

For each database setting, in order:

```
--db-* / --database-url / --sqlite flag
  → DATABASE_URL / PG* / FELDSPAR_SQLITE environment
    → the selected environment's section in feldspar.toml
      → the default (libpq's, or localhost:5432)
```

**With one deliberate inversion.** Naming an environment — `--environment
staging` or `FELDSPAR_ENV=staging` — is an *instruction*, so for that run the
section is authoritative and the ambient `DATABASE_URL` and `PG*` are ignored
entirely. An operator who asks for staging on a box where `DATABASE_URL` points at
production gets staging. Explicit `--db-*` flags still win over everything.

That inversion is why the systemd unit names `--environment production` even
though it is the file's default.

`feldspar serve` prints which environment it connected with, and from which file,
on every start. If the database is unreachable or misconfigured it exits
immediately with an error naming the target — the password is never printed — and
will not boot in a half-working state.

---

## 5. Environment variables

### 5.1 Read at run time

| Variable | Read by | Meaning |
|---|---|---|
| `DATABASE_URL` | every command | full connection string. Overridden by `--database-url`, ignored when an environment is named (§4.3) |
| `PGHOST` `PGPORT` `PGUSER` `PGPASSWORD` `PGDATABASE` | every command | the individual connection parts, matching libpq's own names |
| `FELDSPAR_SQLITE` | every command | use this SQLite file as the primary database |
| `FELDSPAR_CONFIG` | every command | read this configuration file instead of searching. The file must exist |
| `FELDSPAR_ENV` | every command | which `[environments.NAME]` section to use. As explicit as `--environment` |
| `SC_DATA_DIR` | the server | base directory for data the server owns: git clones for git-backed file stores, suggested local-store directories, the Python virtual environment, installed modules. Overrides the platform default |
| `HOME` | the server | used to derive the data directory when `SC_DATA_DIR` is unset. On Linux: `$XDG_DATA_HOME/feldspar`, else `~/.local/share/feldspar` |
| `XDG_CONFIG_HOME` | every command | where the user configuration directory is, for the `feldspar.toml` search |
| `XDG_DATA_HOME` | the server | where the user data directory is, when `SC_DATA_DIR` is unset |
| `APPDATA` `PROGRAMDATA` `LOCALAPPDATA` | every command | the Windows equivalents of the two above |
| `PATH` | the server | where `npm`, `python3` and `typescript-language-server` are found — and `make` and the C++ compiler a Stan compile runs (§9.1) |
| `CMDSTAN` | the server, `feldspar cmdstan status` | the CmdStan directory, when `--cmdstan` (or `cmdstan` in the file) is not given (§9.1) |
| `CXX` | the server | the C++ compiler CmdStan's makefiles use. Passed through to the compile |
| `STAN_*` | the server | CmdStan's own variables, passed through to its processes. Nothing else of the server's environment is (§9.6) |

A daemon started with a **scrubbed environment** — no `SC_DATA_DIR`, no `HOME`,
no `XDG_DATA_HOME` — is an error rather than a fallback to the current directory,
which would scatter clones wherever the process happened to be started. That is
why the unit sets both `HOME` and `SC_DATA_DIR`.

### 5.2 Set by systemd, read by the server

Not yours to set; listed so that a `Type=notify` unit that misbehaves is
diagnosable. The whole protocol is a `sendto` on an `AF_UNIX` datagram socket —
there is no `libsystemd` dependency and no feature flag.

| Variable | Meaning |
|---|---|
| `NOTIFY_SOCKET` | where `READY=1`, `STATUS=…`, `EXTEND_TIMEOUT_USEC=…` and `STOPPING=1` are sent. Unset off systemd, and then every one of those is a no-op |
| `WATCHDOG_USEC` | the interval `WatchdogSec` configured. The server sends `WATCHDOG=1` at half of it |
| `WATCHDOG_PID` | which process the watchdog is for |

Nothing here ever fails the server: a socket that has gone away or a malformed
`WATCHDOG_USEC` is reported on stderr — where the journal is already reading —
and otherwise ignored.

### 5.3 Read at build time only

These are read by `cargo build`. **Putting them on the run command has no
effect.**

| Variable | Effect |
|---|---|
| `SC_BUILD_ADMIN` | set to `0`, `false`, `False` or `FALSE` to skip building the five front-end bundles, leaving a Rust-only build that needs no JS toolchain. Any other value, and leaving it unset, builds them |
| `SC_BUNDLE_PREFIX` | absolute path the artifact will be *installed* at. The recorded bundle paths become `$SC_BUNDLE_PREFIX/ui/admin/dist`, `.../ui/ide/dist`, `.../ui/saltcorn-ui/dist`, `.../ui/builder/dist`, `.../ui/analytics/dist` and `.../plugins`, so they describe the target machine rather than the build machine. This is what `build-static.sh --prefix` sets |
| `SC_ADMIN_BUNDLE_DIR`, `SC_IDE_BUNDLE_DIR`, `SC_SALTCORN_UI_BUNDLE_DIR`, `SC_BUILDER_BUNDLE_DIR`, `SC_ANALYTICS_BUNDLE_DIR`, `SC_PLUGINS_DIR` | the compile-time paths the five bundles and the bundled-module catalog are recorded at, set by the build script |

**What `SC_BUILD_ADMIN=0` (and `--no-ui`) costs, bundle by bundle.** The admin UI can
be supplied at run time with `--static-dir`. The IDE and Saltcorn UI cannot: their
paths have no flag. For Saltcorn UI that means every application whose framework is
`saltcorn-ui` fails to **mount** — once, at boot or on save, with an error naming the
application, the missing bundle and `SC_BUILD_ADMIN` — and is not served, while every
other application is. The restore of a Saltcorn 1 backup still imports its views and
pages (they are rows), and they render as soon as a binary with the bundle serves the
same database. **The builder** degrades rather than failing: layouts can no longer be
edited, `/builder/…` answers a page saying the server was built without it, and the admin
UI shows each layout as JSON with the same sentence.

If the browser shows *"The Saltcorn admin UI is not built"*, the binary was built
with `SC_BUILD_ADMIN=0`. Either restart with `--static-dir ui/admin/dist` (after
`npm run build` in `ui/admin`), or rebuild with the variable unset. `--static-dir`
overrides the admin bundle at run time; **the IDE's path has no flag.**

### 5.4 Read by the test harness

Only relevant on a development machine (README §11).

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | the maintenance connection per-test databases are created and dropped from. Overrides the `test` environment in `feldspar.toml` |
| `SC_TEST_TEMPLATE` | the template each per-test database is cloned from; it must be **empty**, since every test inherits whatever is in it. Overrides `test_template` in the file. Leave it unset to use Postgres's `template1`; set it on a machine whose `template1` has a stale collation version |
| `SC_TSC` | path to a TypeScript compiler for the generated-client type-check tests |
| `SC_TEST_NPM`, `SC_TEST_MQTT_BROKER` | opt in to the tests that need a real npm registry and a real MQTT broker |
| `SC_PRINT_SKILL` | print the generated `SKILL.md` during its test, for reading it |

---

## 6. Reloading without a restart

### 6.1 What `SIGHUP` does

What a mounted application serves is a **snapshot**: the bytes the server read out
of the build's output directory, answered from memory on every later request. So a
developer — or a coding agent — who runs `npm run build` changes the disk and
changes nothing a browser can see.

`SIGHUP` is the cheap third way, between "press Build in the admin UI" and
"restart the service":

```bash
npm run build && pkill -HUP feldspar
```

It **vacates the serving cache** — every mounted app's bundle is re-read from its
output directory — and reloads the definitions around it: the catalog
(re-introspected, overlays and all) and every stored application row, which is
what rebuilds the API providers. A table added, a column added or a custom SQL
query saved by another process is being served by the time the signal returns.
Applications whose row has gone are unmounted.

It **runs no bundler and no installer.** That is the point: the caller has just
built, and re-running `npm install && npm run build` would duplicate the work that
prompted the signal. An application that has never been built has nothing to load
and says so.

It is **best-effort on a server that is already serving**. An app whose bundle has
gone missing is reported and **keeps serving its previous version** — the remount
happens only after the new bundle has loaded — and a catalog that will not
re-introspect is reported and is not a reason to stop answering requests. The
server logs what it reloaded, what failed, and how long each half took, which is
usually a few milliseconds.

**Sessions survive a reload.** They live in the server's memory and a restart
invalidates every one of them; `SIGHUP` leaves them alone, which is exactly what
makes a build → reload → screenshot loop worth having (§7.5).

### 6.2 What it does not reload

These are assembled once at boot and shared into the scheduler and the catalog's
write path, so swapping them is a larger change than a reload. Each already has an
admin API that updates the live set in place, so the restart is rarely what you
want:

- the **trigger set** and the **agents**,
- the **LLM providers**,
- **file-store connections** — a store's *contents* are read live, but a store
  definition added since boot is not connected here,
- **database connections** — a connected database is re-introspected with the
  catalog, but one defined since boot is not dialled here,
- everything read at boot: TLS settings, the ACME order's list of names, and every
  `serve` flag.

`SIGHUP` is Unix only. On Windows the admin UI's Build button is the whole story.

### 6.3 Sending it

From the machine, when the server runs under its own name:

```bash
pkill -HUP feldspar
```

If `pkill` finds nothing, the server is running under another name or another
user. By PID:

```bash
sudo kill -HUP "$(systemctl show -p MainPID --value feldspar)"
```

Through systemd, to the **main process only** — which matters, because the unit's
cgroup may also hold an `npm install` the server started, and that child should
not get the signal:

```bash
sudo systemctl kill -s HUP --kill-whom=main feldspar     # --kill-who on systemd < 252
```

The cleanest arrangement is to teach the unit about it once, so `systemctl reload`
means the right thing:

```bash
sudo systemctl edit feldspar
```

```ini
[Service]
ExecReload=/bin/kill -HUP $MAINPID
```

```bash
sudo systemctl daemon-reload
sudo systemctl reload feldspar
journalctl -u feldspar -n 20 --no-pager        # what it reloaded, and how long it took
```

`SIGHUP`'s default disposition is to **terminate**, so a build of the server that
predates the reload handler dies on that signal instead of reloading. The handler
is installed synchronously, before the listeners are bound, and says so once at
boot:

```
feldspar: SIGHUP reloads the catalog and the applications in place — `pkill -HUP feldspar` after a build, no restart needed
```

If that line is not in the journal, do not send the signal to production — either
this build predates the handler, or registering it failed, which is reported on
the line above as `SIGHUP reloading is unavailable`.

---

## 7. Claude Code over the MCP server

An application built here is **two halves**. One is code in a git repository:
pages, routes, styling, the calls the browser makes. The other is configuration in
the server's database: the tables and their fields, who may read and write them,
the triggers, the workflows, the agents, the applications themselves.

A coding agent working in your repository has the first half through the
filesystem and, by default, no access at all to the second. Asked to *"add a
`priority` field to `tasks` and escalate a task when it goes high"*, it can write
every line of the front end and do neither of the two things that make the feature
work.

The **administration MCP server** is how it reaches the second half: served by the
same process, on one route, behind a bearer token an admin mints and can revoke.
[`tutorial-mcp.md`](tutorial-mcp.md) walks it end to end; this section is the
operational shape of it.

### 7.1 Turning it on

Off by default, and **off means absent**: the route answers `404` and does not so
much as read the token table. Two settings, in **Settings → Development**, both
effective on the next request with **no restart** — the moment you want the server
on is the moment the server is already running.

| Setting | Key | Default |
|---|---|---|
| Administration MCP server | `mcp_enabled` | off |
| MCP from this machine only | `mcp_loopback_only` | **on** |

From the admin UI, or from a terminal that holds the database:

```bash
feldspar set-cfg mcp_enabled true
feldspar set-cfg mcp_loopback_only false   # only if the agent is not on this host
```

Leave `mcp_loopback_only` on. The usual arrangement is an agent running beside the
server or reaching it down an ssh tunnel the developer made:

```bash
ssh -N -L 3032:127.0.0.1:3032 you@app1     # the agent then talks to localhost:3032
```

Turn it off only when the agent genuinely is elsewhere, and read the help text
about proxies first — behind a reverse proxy every peer looks local.

### 7.2 Minting a token

The panel below those switches. A label, an expiry in days, and six grants:

| Grant | What it allows |
|---|---|
| `allow_create` | create tables, fields, triggers and custom SQL queries |
| `allow_edit` | change existing ones |
| `allow_drop` | drop tables and fields, delete triggers and queries |
| `allow_access_changes` | change role floors, ownership formulae and row-level security |
| `allow_triggers` | work on triggers at all |
| (applications) | work on applications' custom SQL queries |

`allow_access_changes` is the one to think hardest about: it changes what *every*
user of the deployment can reach. `allow_drop` at least announces itself.

The token is shown **once**, prefixed `fspk_` — which is what makes it
recognisable in a paste and greppable by a secret scanner — with the registration
line built around it. The server keeps only a SHA-256 hash, so a lost token is
revoked and replaced rather than recovered.

**Two sentences to read before minting one.** *This token is an administrator*: it
runs with the full authority of the admin who minted it, bounded only by those six
boxes, and every call is authorized exactly as that person's own session would be.
*Revoking it is the only way to take it back*: there is no session to expire and
no browser to close.

### 7.3 Registering it

One line, in the project directory of the application's repository:

```bash
claude mcp add --transport http feldspar http://localhost:3032/mcp \
  --header "Authorization: Bearer fspk_…"
```

Start `claude` there and type `/mcp`; `feldspar` should be listed as connected,
offering about twenty-five tools.

Two things in that line are decisions rather than defaults:

- **`--transport http`.** The server speaks streamable HTTP on one route and opens
  no server-initiated stream — every tool is request/response, so an SSE channel
  would be a connection held open for no traffic. There is no stdio transport.
- **The header, and only the header.** `Authorization: Bearer` is the *only*
  credential the route accepts. A session cookie on it is **ignored, not
  honoured** — which is the whole confused-deputy story: a page you happen to
  visit cannot set an `Authorization` header cross-origin without a preflight this
  server will not answer, so no site can reach the administrative surface through
  the admin session you happen to be signed into.

### 7.4 Creating tables

Ten composite tools sit over the administrative surface, plus the ones generated
from tagged endpoints:

| Tool | For |
|---|---|
| `describe_schema` | every table, its fields and types, its access rules, foreign keys both ways |
| `edit_schema` | an **ordered list** of schema operations, applied as one transaction |
| `describe_triggers`, `describe_action`, `save_trigger`, `delete_trigger` | the trigger half |
| `describe_code_api` | the JavaScript API a `run_js_code` body or a `javascript` API query can call — `db`, `fetch`, `fs`, `trigger` |
| `describe_applications`, `save_api_query`, `delete_api_query` | applications and their custom SQL |

Ask for the whole data model at once:

> Create four tables for a small CRM: `companies`, `contacts` (belonging to a
> company), `deals` (belonging to a company, with a stage and an amount) and
> `notes` (belonging to a deal). Contacts and deals should be readable by any
> signed-in user.

**`edit_schema` is called once**, not once per table. It takes an ordered list
applied as a single transaction, because a schema is a set of connected tables and
a per-operation tool turns a twelve-table domain into forty round trips. A foreign
key in the list may point at a table created earlier in the same list. **If any
operation is refused, none of them happened.**

The result names what moved. A schema change re-projects every mounted application
that serves an affected table and rewrites its generated TypeScript client on
disk, at once and with no restart — but it runs **no bundler**, so the result says
which applications are now serving a bundle compiled against the old schema:

```json
{
  "applied": true,
  "fields_added": 1,
  "applications": [{ "id": "…", "subdomain": "todo", "wants_build": true }],
  "notes": ["Re-projected, with the generated client rewritten: `todo`. No bundler was run … call `buildApplication` with the `id` above."]
}
```

**Refusals are readable, and they are whole.** A `drop_table` slipped into a batch
a token was not granted refuses the entire batch:

```
operation 1 (drop_table on `task_audit`): not permitted to drop a table; the whole
batch was refused and nothing was applied. Turn on `allow_drop` to allow it.
```

Named by index as well as by name, saying what was *not* done, and naming the
checkbox that would allow it — so you get "the token isn't granted drop, do you
want to mint one that is?" instead of a retry loop against `Forbidden`. An area
that is off behaves differently again: `allow_triggers` unticked does not refuse
the trigger tools, it **removes them from the listing**, because a tool a model can
see is a tool it will try.

Two things are not offered whatever you tick: **your source files** — the agent has
the repository and an editor — and **row data**. There is no `list_rows` tool and
there will not be one; this surface describes the shape of a database, never its
contents.

### 7.5 Coding a React application

The two halves together. Create the application in the admin UI once —
**Applications → New application**, framework **React**, a file store, a project
directory, the tables it serves, and an API provider mount — and the server
scaffolds a complete Vite + React + TypeScript project generated against those
tables. Press **Build** and it installs dependencies, type-checks, bundles, and
serves the result at `<subdomain>.<base-domain>` with no restart.

From there the agent works in the project directory:

```
todo/
  package.json  vite.config.ts  tsconfig.json  index.html
  src/
    main.tsx  App.tsx  routes.tsx  auth.tsx  Login.tsx  app.css
    pages/Tasks.tsx
    feldspar/            ← generated, rewritten on every build, DO NOT EDIT
      client.ts  helper.ts  hooks.ts  store.ts  schema.sql
      README.md  SKILL.md
```

**`src/feldspar/SKILL.md` is written for the agent**, on the same schedule as the
client and for the same reason: it describes the same application. It says which
half is in the repository and which is in the server, lists the tools that reach
the second half with the grant each needs, and — as loudly — the ones that
deliberately do not exist. Its tool list is derived from the same place the server
projects the tools from, so it cannot drift. Copy it where Claude Code will load
it automatically:

```bash
mkdir -p .claude/skills/feldspar
cp src/feldspar/SKILL.md .claude/skills/feldspar/SKILL.md
```

The listing in it is generated with **every** grant, because a repository is not a
token — the same project may be worked on with two tokens granted different
things. What a given token actually has is its `tools/list`, and the file says so.

**Everything outside `src/feldspar/` is yours** from the moment it exists: the
scaffold writes those files once and never touches them again. Adding a column
updates the *types*; putting it on the page is the agent's job.

Give the agent a session so it can check its own work, since most screens require
a signed-in user and a browser driven by a script otherwise photographs the
sign-in page:

```bash
feldspar auth token --app todo --role Reviewer
```

It asks for no password — the command already holds the database, which is more
authority than any password buys — so what it needs is a *user*: `--admin` (the
first admin), `--role NAME` (the first user holding it), or `--email` (that person
exactly). **Give the agent its own account** at the lowest role that can see the
screens it is looking at. The session is minted by the running server from a
single-use grant good for two minutes; it forges nothing and can do exactly what
that account can. The default output is `.feldspar-session.json` in Playwright's
`storageState` shape, written `0600` and already in the project's `.gitignore`;
`--format netscape` writes a `cookies.txt` for `curl --cookie` instead.

**A session file is a password.** It is a logged-in browser for whoever has it.

The loop the agent then runs is the one §6 exists for:

```bash
npm run build && pkill -HUP feldspar
# then screenshot http://todo.localhost:3032/
```

and, when the schema changed under it, `buildApplication` over MCP instead —
which regenerates the client, runs `npm run build`, and serves the result on the
subdomain. A project that does not compile comes back as a **result** (`built:
false`, with the tools' output and the diagnostics parsed out of it), not as a
refusal: a model told only "the build failed" cannot fix anything.

### 7.6 Watching it, and taking it back

A credential that lives ninety days is defensible when its use is visible in the
log stream you are already watching. Every tool call writes one line at `info`,
naming the **label** — never the token, never its hash:

```
MCP [claude-code on my laptop] describe_schema ok in 7ms
MCP [claude-code on my laptop] edit_schema ok in 41ms
MCP [claude-code on my laptop] edit_schema refused in 3ms: operation 1 (drop_table on `task_audit`)…
```

```bash
journalctl -u feldspar -f | grep '^MCP'
```

Set **Log verbosity** to `verbose` in the same Settings → Development section and
the call arguments are logged too.

**Revoke** on the token's row ends it: the row stays, marked, because a revocation
is a thing that happened and that list is where it is seen to have happened. The
next call the agent makes fails. The same is true if the token expires, if the
user who minted it is deleted, or if they stop being an administrator — the
credential names a **user**, and it is checked against that user on every call.

---

## 8. Day-to-day operations

### 8.1 Health

```bash
curl -s http://127.0.0.1:3032/health      # {"status":"ok"}
curl -s http://127.0.0.1:80/health        # a setup-host.sh install binds 0.0.0.0:80
```

An unauthenticated, fixed route, on whatever `bind` is. A `200` means the process
booted and is accepting requests.

```bash
systemctl status feldspar
journalctl -u feldspar -n 100 --no-pager
journalctl -u feldspar -f
```

### 8.2 Settings from a terminal

The settings an admin edits under **Settings** are rows in the primary database,
not a file. These two commands need the database and nothing else — no running
server, no session, no browser:

```bash
feldspar get-cfg                                   # every setting, key=value
port=$(feldspar get-cfg https_port)                # one value, ready to capture
feldspar set-cfg smtp_host smtp.example.com
feldspar set-cfg ssl_certificate < fullchain.pem   # multi-line values on stdin
```

The value's type comes from the key and is checked before anything is written, so
`set-cfg https_port yes` is a message and not a stored string. Secrets are redacted
in the listing (`smtp_password=••••••••`) because a listing ends up in scrollback
and in CI logs; naming the key prints it in full. **Nothing is restarted** — when a
setting takes effect is the setting's own business: the logging switches are
immediate, the SMTP transport is read per message, and the TLS settings are read
at boot.

### 8.3 The bound on a model dataset

A model's dataset is a `SELECT` an administrator wrote, and a fit holds the whole
answer in memory. `--model-max-rows` (default **200 000**) is the ceiling:

```bash
feldspar serve --model-max-rows 500000
```

The count is asked for **before** the rows, so a dataset over the bound is refused for
the cost of one `COUNT(*)` — *"the dataset selects more than 200 000 rows; add a filter
or raise `--model-max-rows`"* — rather than by the OOM killer after a partial read. The
message goes onto the failed instance, where the admin will look for it.

It is the only bound a smartcore or Python fit has. **Those fits have no cancel**:
stopping one means stopping a smartcore call or a CPython call mid-flight, and neither
can be interrupted safely (the technical design's §15.2 says why for Python). Size the
flag for the memory the process has, and remember that each column is materialised as a
boxed vector before the numeric matrix is built. A Stan fit reads its datasets under the
same bound and has more of its own, and it **can** be cancelled, because it is a set of
processes (§9).

### 8.4 Stopping

`SIGTERM` — what `systemctl stop` and an orchestrator send — shuts down
gracefully, and the unit is `deactivating` for the length of the drain rather than
looking hung. Ctrl-C does the same interactively.

### 8.5 Common failures

| Symptom | Cause and fix |
|---|---|
| `error: connecting to database …` at startup | unreachable or wrong credentials. The message names the target without the password. Confirm `psql` reaches the same URL, and that the database exists — the server does not create it |
| startup error about the `users` table or permissions | the connecting role cannot create tables. Make it the **owner** of the database |
| "The Saltcorn admin UI is not built" | the binary was built with `SC_BUILD_ADMIN=0`. It is a build-time variable (§5.3): restart with `--static-dir ui/admin/dist`, or rebuild with it unset |
| An application logs "this server was built without the Saltcorn UI bundle" at boot, and its subdomain serves nothing | its framework is Saltcorn UI and the binary was built with `SC_BUILD_ADMIN=0` or `--no-ui` (§5.3). No flag supplies the bundle at run time: rebuild with the variable unset. Other applications are served meanwhile |
| `/builder/…` says "This server was built without the builder (ui/builder)", and a view's layout step or a page shows JSON instead of **Open in builder** / **Edit** | the binary was built with `SC_BUILD_ADMIN=0` or `--no-ui` (§5.3). Everything else about Saltcorn UI works. Rebuild with the variable unset |
| the builder's canvas stays empty, or its console reports a Content-Security-Policy violation | the builder document's policy is fixed: the admin UI's, plus its application's origin in `img-src` (README §6, *The builder*). A violation means the bundle loaded something the policy does not name, which is a bug to report with the console line, not a setting to change. An image that will not show usually means the application's public origin is wrong: the canvas's `/files/serve/…` images are redirected there |
| "the Saltcorn UI bundle is missing its view runtime" | the recorded `ui/saltcorn-ui/dist` exists but has no `view-runtime.js` — an interrupted build or a partly copied release tree. Run `npm ci && npm run build` in `ui/saltcorn-ui`, or reinstall the tree, then `SIGHUP` |
| `ETXTBSY` / `Text file busy` during an update | the service is running and holds its own executable open. `systemctl stop feldspar` first (§3.2), or use `--deploy`, which stops and starts it for you |
| a Python trigger says the server was built without Python | it was. `--features python` is build-time and no flag substitutes for it. Settings → Development names which of the four states this process is in |
| `error while loading shared libraries: libpython3.x.so` | a Python-feature binary on a host with no matching `libpython`. Install it, or run a binary built without the feature. `abi3` means any CPython 3.11+ will do, but the *soname* is version-specific |
| login does not stick behind HTTPS | add `--secure-cookies` (or `secure_cookies = true`). Conversely, do **not** set it for plain-HTTP local development, or the browser drops the cookies |
| an application 404s after an update | it failed to build at boot and was skipped rather than taking the server down. `feldspar build-app <subdomain> --environment production` prints the bundler's own diagnostics |
| `POST /mcp` answers 404 with a valid token | `mcp_enabled` is off, and off means absent (§7.1) |
| MCP calls refused from another machine | `mcp_loopback_only` is on, which is its default |
| a model fit says "the server restarted while this fit was running" | it did. A fit is a spawned job whose only record is its instance row, so boot marks a `fitting` row failed rather than leaving it running for ever (§3.5). Press **Fit** again |
| every read of a table fails with "`estimated_price` of `houses` could not be computed … has no active fit" | a calculated field calls `predict("…")` and its model has no active fit, perhaps because the active one was deleted. Activate a fit on the model's Fits list. A read fails rather than answering null, so a model a field depends on must always have an active fit. A nightly `fit_model` with `activate: if_clean` never deactivates one |
| a fit fails with "the dataset selects more than … rows" | the dataset is over `--model-max-rows` (§8.3). Add a filter to the dataset, or raise the flag |
| the model editor offers only `t_test` and `anova` | the binary was built `--no-default-features`, so the smartcore providers were compiled out (§1). It is a build, not a setting |
| the `stan` provider says "CmdStan was not found" | the server looked where §9.1 says and found none, or found one it refused (too old, not built, a named directory that is not there). The startup log has the same sentence. Under systemd, `~` is `/var/lib/feldspar`, not your home |
| a Stan fit says `queued` for a long time | every chain process the node allows is taken by other fits (§9.2). It starts when one finishes; raise `--stan-max-processes` if the machine has the cores |
| a Stan fit fails with "more than the … allowed (`--stan-max-data-values`)" | the bound data is too large for the ceiling (§9.4). Filter the datasets or raise the flag |
| a Stan fit finishes with "the draws … were not kept" | they were over `--stan-max-draws-bytes` (§9.4). The summary and diagnostics are there; to keep the draws, `thin`, `exclude_variables`, or raise the flag |
| `getModelDraws` refuses with "… numbers" and suggests `thin` | the answer would be over `--stan-max-draws-response` (§9.4). Ask for fewer elements, chains or draws |
| `/analytics/` says "the Analytics UI bundle is not built" | the binary was built with `SC_BUILD_ADMIN=0` or `--no-ui` (§5.3), or `ui/analytics/dist` is missing under the install prefix — a release tarball from before the packaging staged it did not include it; rebuild and reinstall. Rebuild with the variable unset, or run `npm ci && npm run build` in `ui/analytics` for a source tree |
| `feldspar demo analytics` says "the database already has `houses` …" | the demo never touches a table that is there. Use `--replace` to drop and remake its tables, or run it against another database (§8.6) |

### 8.6 Demo data for the Analytics UI

```bash
feldspar demo analytics [--replace] [database flags]
```

makes tables for trying the Analytics UI (`/analytics/`, linked from the admin sidebar):
`neighbourhoods` (5 rows), `houses` (200, a key to `neighbourhoods`, some unsold with no
`price`), `viewings` (a key to `houses`, with dates), `patients` (90) and `measurements` (a key
to `patients`, a blood pressure `before` and `after` a treatment), and `events` (a million rows,
generated by the database in one statement: a few seconds, and about 90 MB on Postgres). The
data is synthetic and generated from a fixed seed, so every run on every machine makes the same
rows, on Postgres or SQLite. It also makes three datasets for the Data explorer — `Houses`,
`Measurements` and `Events` — unless a dataset of that name is already there. It takes the same
database flags as `feldspar serve` and writes where the server would read.

It refuses to touch a table that is already there, naming it. `--replace` drops and remakes
the demo's six tables — and only those, so anything else in the database, including datasets
and models that read them, is left alone (a dataset over a dropped table reports its error until
the table is back), and the demo's datasets are kept as they are. Run it on a scratch database,
not on one whose `houses` table is yours.

---

## 9. Bayesian models with Stan

A model whose provider is **Stan** is a Stan program the administrator wrote, compiled and
sampled by [CmdStan](https://mc-stan.org/docs/cmdstan-guide/) on the server. Nothing of
CmdStan is linked into `feldspar`: it is a directory, a `make` and a C++ compiler, found at
run time, so both artifacts of §1 can fit Stan models and whether *this host* can is decided
by what is installed on it. A host with no CmdStan runs everything else; the `stan` provider
is listed saying why it is unavailable.

### 9.1 Installing CmdStan

What the host needs, beside the server:

| | Needed for | Size |
|---|---|---|
| CmdStan **2.33 or newer** | every Stan model | about **1.2 GB** built (2.40.0) |
| `make` and `g++` (or `clang++`) | compiling each program | Debian's `build-essential` |
| memory | the build, and each compile | **1–2 GB per job** |

`feldspar cmdstan` does the install, from a shell, on purpose — the server never downloads
or builds anything on its own:

```bash
sudo apt install build-essential                 # make and g++
feldspar cmdstan install                         # the latest release, into ~/.cmdstan, make -j1
feldspar cmdstan install --version 2.40.0 --dir /srv/cmdstan --jobs 2
feldspar cmdstan status                          # what the server will find, and whether it can compile
```

`install` downloads the release tarball from GitHub, unpacks it into `<dir>/cmdstan-<version>`
and runs `make build`. That takes several minutes, and **`--jobs` defaults to 1** because
each job takes 1–2 GB: on a host with `systemd-oomd`, a parallel build can have the whole
session killed rather than fail. An install that fails or is interrupted removes what it had
unpacked. `status` exits non-zero when Stan cannot be used, so a provisioning script can test
for it.

**Where the server looks**, first match wins:

1. `--cmdstan DIR` (or `cmdstan = "DIR"` in the environment, §4.2);
2. `$CMDSTAN`;
3. the newest `~/.cmdstan/cmdstan-*` — where `feldspar cmdstan install` and cmdstanpy's
   `install_cmdstan` both put one, so an existing install is picked up.

A directory named by 1 or 2 that is missing, not a CmdStan, not built or older than 2.33 is
**refused by name**, not stepped over for the one in `~/.cmdstan`: a fit on a version nobody
chose is worse than a provider that says what is wrong. The server says at startup what it
found:

```
feldspar: Stan models will use CmdStan 2.40.0 at /var/lib/feldspar/.cmdstan/cmdstan-2.40.0 (the newest under ~/.cmdstan), up to 4 chain process(es) at once, compiling into /var/lib/feldspar/stan-cache
feldspar: Stan models are unavailable: CmdStan was not found — …
```

**Under the systemd unit of §2.4**, `~` is the service account's home, `/var/lib/feldspar`,
and that is the only directory the server may write. So install CmdStan *as* the service
account, where the server looks by default and where it can write:

```bash
sudo -u feldspar -H feldspar cmdstan install
sudo -u feldspar -H feldspar cmdstan status
```

A CmdStan installed under your own home is invisible to the unit (`ProtectHome=true`). One
installed elsewhere as root (`--dir /opt/cmdstan`) works if it is complete — it is only read
at run time — but must be named with `cmdstan` in the file, and upgrading it is then yours to
do. Upgrading CmdStan is an install of the new version beside the old one; the server takes
the newest at its next start, and every program is recompiled on its next fit, because the
CmdStan version is part of the compile cache's key.

### 9.2 The process budget

Every chain of every fit is **one CmdStan process**, and every chain process on the node
draws from one budget: `--stan-max-processes` (default: half the available CPUs, at least
one). A fit's own `parallel_chains` setting caps it further. A chain waiting for the budget
leaves its fit saying `queued` in the model editor; nothing is refused.

A chain is single-threaded (the server compiles without `STAN_THREADS`), so the budget is
roughly the cores Stan may keep busy. The default leaves half of them for the server, the
database and whatever else is on the host; a host that does little but fit models can raise
it.

Fits are jobs, as every model fit is (§8.3): a fit survives the browser, not a restart. The
chains are killed with the server (they are its children, in their own process group, with a
parent-death signal on Linux), the instance is marked failed at the next boot, and the next
boot removes their scratch directories. A running Stan fit **can** be cancelled from its
instance screen, from any node, and `max_runtime_minutes` in its configuration stops one that
runs too long.

### 9.3 The compile cache

A Stan program is compiled to a native executable — a C++ compile, **about a minute and
1–2 GB of memory** — once per node, and kept in `--stan-cache-dir` (default
`stan-cache` in the platform's data directory, `/var/lib/feldspar/stan-cache` under the unit).
An entry is keyed by the SHA-256 of the program and every file it includes, the CmdStan
version and the compile options, so editing any of them, or upgrading CmdStan, is a new entry,
and two models running the same program share one. An entry is a few megabytes (2.5 MB for the
radon model).

**One compile at a time per node**: a second fit of the same program waits for the first
compile and then finds it cached; a fit of another program waits its turn. The **Compile**
button in the model editor warms the cache without fitting.

Nothing prunes the cache — old entries are only disk. Deleting the directory, or any entry in
it, is always safe while no compile is running: the next fit recompiles. A compile interrupted
by a restart leaves a `*.building-*` directory that the next boot removes.

### 9.4 How big a fit can get

Four ceilings, each a server flag because each is about this host's memory, and each refused
**by name** — the message names the flag:

| Flag | Default | What it bounds | Over it |
|---|---|---|---|
| `--model-max-rows` | 200 000 | rows one dataset (the main one, or a related one) may select | the fit fails before anything is read (§8.3) |
| `--stan-max-data-values` | 20 000 000 | numbers in the bound data file, all variables together | the fit fails before compiling, naming the largest variables; **Preview data** says so too |
| `--stan-max-draws-bytes` | 1 GB | draws a fit may store in `_fd_model_draws` | refused before sampling when the size can be computed from the program; otherwise the fit finishes, **keeps its summary and diagnostics**, and discards the draws with a warning |
| `--stan-max-draws-response` | 2 000 000 | numbers in one `getModelDraws` answer | the request is refused with the arithmetic, suggesting `thin` or fewer elements |

And one that is a trade rather than a ceiling: **`--stan-summary-max-elements`** (default
1 000). A generated quantity with more elements than this — a `y_rep` over every row — is not
summarised when the fit finishes; the model editor summarises it on demand from the stored
draws. `0` summarises every generated quantity on demand. On demand needs the draws: a
generated quantity over the ceiling whose draws are not kept (§9.5) is not read back from
CmdStan at all.

### 9.5 Where the draws live, and what they cost

A posterior is its draws, and they are rows: **`_fd_model_draws`**, one row per element per
chain, the iterations as a JSON array, in the primary database. They are written in the same
transaction that marks the instance fitted and deleted with it, so every node reads them, and
a fitted instance always has all of them.

That makes them the largest thing a Stan user adds to the database. **About 12 bytes per
number** is the rule of thumb: the radon model (4 chains × 1 000 draws × ~1 100 elements) is
about 50 MB per fit, and a program that saves a 50 000-element `y_rep` is 100 times that.
Three ways to keep it down, all in the model's configuration:

- `thin`, or fewer `iter_sampling`;
- `exclude_variables`: variables whose draws are not kept (their summary is);
- `keep_draws: false`: keep the summary and the diagnostics, computed from every draw, and
  discard the draws. The traces and histograms are then unavailable, and write-back works
  from the summary.

And one habit: **delete old instances.** Every fit is an instance, and an instance keeps its
draws until it is deleted. `SELECT pg_size_pretty(pg_total_relation_size('_fd_model_draws'))`
is the number to watch.

While a fit runs, CmdStan writes its CSVs to a **scratch directory** under the system
temporary directory (`feldspar-stan/`; the unit's private `/tmp`). It is about the size of
the draws as text, is removed when the fit ends, and a boot removes any a crash left behind.

### 9.6 The raw run, and backups

A model may also name a **runs store** — a file store — in its configuration. Then each fit
publishes CmdStan's own output there, under `<runs_dir>/<model>/<instance id>/`: the program
as fitted, `data.json`, the coordinates, the sampler's configuration, and each chain's CSV,
gzipped, with its log. That is what **Download run** zips for `cmdstanpy.from_csv` and the
exact reproduction of the run. It is optional, and without it Download run builds per-chain
CSVs from `_fd_model_draws` instead.

In a git-backed store, a `.gitignore` of `*` is written into `runs_dir` on first use, so
megabytes of CSV never become a commit. Deleting an instance deletes its run directory.

**Backups** include models, instances and **all** of their draws, which is right — an
instance without its draws is half an instance — and is where the size of `_fd_model_draws`
shows up (§9.5). Raw run directories are in a file store, and a file store's backup is the
store's own: a local directory's is your filesystem backup, a git store's is its remote. An
instance whose run directory has gone (a store restored without it, a directory deleted by
hand) still reads its draws and summary; its Download run falls back to the table, with a
`README.txt` saying why.

### 9.7 Security

A Stan program is admin-authored and **compiles to native code that the server runs**. So
writing to the file store a model's program lives in is a way to change what a fit computes —
the same trust as editing a code trigger's body, and the same people should have it. What the
program cannot do is reach past Stan:

- it cannot bring C++ of its own: programs are compiled without `--allow-undefined`, with no
  administrator-supplied `CXXFLAGS`, and `make` is given only the server's own target and
  include path;
- its `#include`s resolve only inside its own file store, laid out in the compile's own
  directory;
- every CmdStan process runs with a **scrubbed environment** — `PATH`, `HOME`, `TMPDIR`,
  `CMDSTAN`, `CXX` and CmdStan's `STAN_*` variables, nothing else — so it inherits no database
  URL, no password and no API key from the server;
- it reads its data from a file the server wrote and writes CSVs the server reads; it has no
  database connection.

Fits, compiles, draws, write-back and **Download run** are admin-only, as every model
endpoint is.

