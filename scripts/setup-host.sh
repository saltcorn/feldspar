#!/bin/sh
#
# Set up a host to run Saltcorn Feldspar: packages, a service account, the
# PostgreSQL role and database, /etc/feldspar/feldspar.toml and a systemd unit.
# This is README §2 (the Debian quick start) as a script, and it is safe to run
# again — anything that already exists is left alone.
#
# Distributions
# -------------
# Four package families, told apart by /etc/os-release (ID, else the first
# family ID_LIKE names, which is what covers Mint, MX, Rocky, CachyOS,
# Manjaro, EndeavourOS and the rest):
#
#   debian   Debian, Ubuntu        apt; PostgreSQL is set up and started by
#                                  the package; Node from NodeSource (§1b)
#   fedora   Fedora, RHEL, Alma,   dnf; `postgresql-setup --initdb`; Chromium
#            Rocky                 from EPEL on RHEL and its rebuilds
#   suse     openSUSE Leap and     zypper; the postgresql unit initialises its
#            Tumbleweed            own data directory on first start
#   arch     Arch, CachyOS,        pacman; `initdb` with peer authentication
#            Manjaro, EndeavourOS  on the socket
#
# Alpine is not one: it runs OpenRC, not systemd, and this script writes a
# systemd unit. On Alpine, install.sh is the whole install and the service is
# the operator's own (TODO-portable-release D9).
#
# No Python package is installed on any of them: the `linux` artifact brings
# its own CPython. Only a host still on the older `debian13-python` artifact,
# whose binary links Debian's libpython3.13, gets Debian's Python packages.
#
# The two ways to get a binary onto the host
# ------------------------------------------
# 1. **Static artifact — no toolchain on the host.** The release tarball carries
#    a copy of *this script*, so a host with nothing on it needs no checkout and
#    no curl-into-a-shell:
#
#        tar -xzf feldspar.tar.gz
#        sudo feldspar-*/install.sh                      # the tree at /opt/feldspar
#        sudo /opt/feldspar/setup-host.sh --domain example.com
#
#    Run from ${PREFIX} like that, the script sees the binary beside it and takes
#    --static as the default; there is nothing to build.
#
#    The same artifact can be pushed from a workstation instead, in a checkout of
#    this repository:
#
#        scripts/build-static.sh --deploy root@host      # builds, copies, installs
#
#    and then, on the host, `sudo /opt/feldspar/setup-host.sh --domain example.com`.
#    Either order works: with --static this script never starts a server whose
#    binary is not there yet — it enables the unit and leaves it stopped, so an
#    earlier setup is followed by `systemctl start feldspar`. Downloaded on its
#    own rather than out of a tarball, it still needs the flag:
#
#        curl -fsSL https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \
#          | sh -s -- --static --domain example.com
#
# 2. **From source on the host — one step, but the host does the building.** The
#    default: rustup, a clone and a release build, which wants a C toolchain,
#    libclang, Node and a few GB of RAM.
#
#        curl -fsSL https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \
#          | sh -s -- --domain example.com
#
# Running it
# ----------
# The script is on the shell's stdin when it arrives down a pipe, so it can ask
# nothing: every choice is a flag, and everything after `sh -s --` is one of this
# script's own. `--help` lists them, and `--dry-run` prints every command and file
# it would write without touching anything, which is worth doing once before
# piping a script off the internet into a shell.
#
#   wget -qO- https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \
#     | sh -s -- --dry-run --static --domain example.com
#
# Run it as root, or as a user who can sudo.
#
# A headless browser
# ------------------
# The in-server coding agent looks at the application it built through a
# headless Chromium (`view_app`, TODO §7b), which the server drives over the
# DevTools protocol and starts itself, as the service account. So the script
# installs one that works under a systemd service user:
#
#   Debian 12/13   apt's `chromium`.
#   Ubuntu         *not* apt's `chromium-browser`, which is a transitional
#                  package whose /usr/bin/chromium-browser only execs the snap,
#                  and a snap does not run under a service account with
#                  ProtectHome/PrivateTmp. Google Chrome's own apt repository
#                  instead (amd64 only; elsewhere the step says so and skips).
#
# A browser already on PATH (chromium, google-chrome) that is not a snap shim is
# kept. The last step starts it once as the service account
# (`--headless --dump-dom about:blank`), so a browser that cannot run there is
# found now rather than at the agent's first `view_app`. `--no-browser` skips all
# of it; the server then logs that `view_app` is unavailable, and why.
#
# What it does *not* do: open a firewall, point DNS at the host, create the first
# admin user or ask for a certificate. Those are README §2.7, and three of the four
# happen in a browser.
#
set -eu

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

MODE="source"           # or "static": a binary the release tarball put there
DOMAIN=""               # base_domain; without one only the admin UI is served
BIND="0.0.0.0:80"
PREFIX="/opt/feldspar"  # must match build-static.sh --prefix

# Run out of an installed artifact — /opt/feldspar/setup-host.sh, beside the
# bin/feldspar that install.sh just put there — a *source* install is not what
# anybody means: the binary is already here, and building a second one would need
# the toolchain the static artifact exists to avoid. So the location of this file
# picks the default, and the prefix follows it, which is also what makes an
# artifact installed somewhere other than /opt/feldspar work with no flags.
# --from-source and --prefix still override; a script arriving on stdin down a
# pipe has no location and changes nothing here.
case "$0" in
    */*)
        if [ -f "$0" ]; then
            SELF_DIR="$(cd -- "$(dirname -- "$0")" && pwd)"
            if [ -x "${SELF_DIR}/bin/feldspar" ]; then
                MODE="static"
                PREFIX="${SELF_DIR}"
            fi
        fi
        ;;
esac
SERVICE_USER="feldspar"
# The one writable path: the service account's home, the unit's HOME and
# WorkingDirectory, systemd's StateDirectory and the only ReadWritePaths. It is
# a single name because those five have to agree — see §2 and §6.
STATE_DIR="/var/lib/feldspar"
DB_NAME="feldspar"
DB_USER="feldspar"
DATABASE_URL=""         # set: an existing database elsewhere, and no local postgres
REPO="https://github.com/saltcorn/feldspar.git"
BRANCH="main"
SRC_DIR="/opt/feldspar/src"
CONFIG_FILE="/etc/feldspar/feldspar.toml"
UNIT_FILE="/etc/systemd/system/feldspar.service"
ENVIRONMENT="production"
START=1
BROWSER=1               # install and verify a headless Chromium for view_app
FORCE=0
DRY_RUN=0

usage() {
    cat <<EOF
Set up this host to run Saltcorn Feldspar: packages, a service account, the
database, the configuration file and the systemd unit.

Usage: setup-host.sh [options]
   or: curl -fsSL https://raw.githubusercontent.com/saltcorn/feldspar/main/scripts/setup-host.sh \\
         | sh -s -- [options]

How the binary gets here
      --static          Do not install a Rust toolchain, clone or build. The binary
                        is expected at ${PREFIX}/bin/feldspar, where the release
                        tarball's install.sh — and \`build-static.sh --deploy\` —
                        put it. Already the default when this script is run from
                        that tree (${PREFIX}/setup-host.sh), which is where the
                        tarball installs a copy of it. The binary may arrive
                        before or after this script; the unit is only started once
                        it is actually there.
      --from-source     Install rustup, clone the repository and build it here.
                        The default when this script is *not* run from an
                        installed artifact. Needs a C toolchain, libclang and RAM.
      --repo URL        Repository to clone (default ${REPO}).
      --branch NAME     Branch to check out (default ${BRANCH}).
      --src DIR         Where to clone it (default ${SRC_DIR}).

The deployment
      --domain DOMAIN   base_domain: applications are served at <app>.DOMAIN, and a
                        wildcard DNS record should point at this host. Without it
                        only the admin UI is served — the server has no way to
                        address an application.
      --bind ADDR:PORT  Listen address (default ${BIND}).
      --prefix PATH     Where the static artifact is installed (default ${PREFIX}).
                        Must be the prefix the artifact was *built* for: the admin
                        UI and IDE paths are compiled into the binary.
      --environment N   Name of the environment in feldspar.toml (default ${ENVIRONMENT}).

The database
      --database-url URL
                        Use an existing PostgreSQL elsewhere, e.g.
                        postgres://feldspar:pw@db.internal/feldspar. No PostgreSQL
                        is installed here and no role or database is created.
      --db-name NAME    Local database name (default ${DB_NAME}).
      --db-user NAME    Local role name (default ${DB_USER}). Peer authentication
                        over the Unix socket, so it matches the service account.

Other
      --user NAME       System account the service runs as (default ${SERVICE_USER}).
      --config PATH     Where the configuration file goes (default ${CONFIG_FILE}).
                        The unit names this file outright, so the two agree.
      --unit PATH       Where the systemd unit goes (default
                        ${UNIT_FILE}); for staging it somewhere else.
      --no-browser      Do not install a headless Chromium. The coding agent's
                        view_app tool is then unavailable until one is installed
                        (or named with \`browser\` in the configuration file).
      --no-start        Create and enable the unit, but do not start it.
      --force           Overwrite an existing ${CONFIG_FILE}
                        or ${UNIT_FILE} (both are kept by default).
  -n, --dry-run         Print every command and file this would write, change
                        nothing. Worth doing once before piping a script off the
                        internet into a shell.
  -h, --help            This message.
EOF
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

need_value() {
    [ $# -ge 2 ] || { echo "error: $1 needs a value" >&2; exit 2; }
}

while [ $# -gt 0 ]; do
    case "$1" in
        --static)        MODE="static"; shift ;;
        --from-source)   MODE="source"; shift ;;
        --repo)          need_value "$@"; REPO="$2"; shift 2 ;;
        --branch)        need_value "$@"; BRANCH="$2"; shift 2 ;;
        --src)           need_value "$@"; SRC_DIR="$2"; shift 2 ;;
        --domain)        need_value "$@"; DOMAIN="$2"; shift 2 ;;
        --bind)          need_value "$@"; BIND="$2"; shift 2 ;;
        --prefix)        need_value "$@"; PREFIX="$2"; shift 2 ;;
        --environment)   need_value "$@"; ENVIRONMENT="$2"; shift 2 ;;
        --database-url)  need_value "$@"; DATABASE_URL="$2"; shift 2 ;;
        --db-name)       need_value "$@"; DB_NAME="$2"; shift 2 ;;
        --db-user)       need_value "$@"; DB_USER="$2"; shift 2 ;;
        --user)          need_value "$@"; SERVICE_USER="$2"; shift 2 ;;
        --config)        need_value "$@"; CONFIG_FILE="$2"; shift 2 ;;
        --unit)          need_value "$@"; UNIT_FILE="$2"; shift 2 ;;
        --no-start)      START=0; shift ;;
        --no-browser)    BROWSER=0; shift ;;
        --force)         FORCE=1; shift ;;
        -n|--dry-run)    DRY_RUN=1; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               echo "error: unknown option $1" >&2; echo >&2; usage >&2; exit 2 ;;
    esac
done

case "${PREFIX}" in /*) ;; *) echo "error: --prefix must be absolute, got ${PREFIX}" >&2; exit 2 ;; esac
case "${SRC_DIR}" in /*) ;; *) echo "error: --src must be absolute, got ${SRC_DIR}" >&2; exit 2 ;; esac
case "${CONFIG_FILE}" in /*) ;; *) echo "error: --config must be absolute, got ${CONFIG_FILE}" >&2; exit 2 ;; esac
case "${UNIT_FILE}" in /*) ;; *) echo "error: --unit must be absolute, got ${UNIT_FILE}" >&2; exit 2 ;; esac

# ---------------------------------------------------------------------------
# Which distribution
# ---------------------------------------------------------------------------

# Read in place of /etc/os-release when set: how the tests stand in for
# another distribution. Each value is read in a subshell, so the file's other
# variables (NAME, VERSION, …) do not land in this script's.
OS_RELEASE="${FELDSPAR_OS_RELEASE:-/etc/os-release}"
os_field() {
    (
        ID="" ID_LIKE="" VERSION_ID=""
        # shellcheck disable=SC1090
        [ -r "${OS_RELEASE}" ] && . "${OS_RELEASE}"
        case "$1" in
            ID)         printf '%s' "${ID}" ;;
            ID_LIKE)    printf '%s' "${ID_LIKE}" ;;
            VERSION_ID) printf '%s' "${VERSION_ID}" ;;
        esac
    )
}
OS_ID="$(os_field ID)"
OS_ID_LIKE="$(os_field ID_LIKE)"
OS_VERSION_ID="$(os_field VERSION_ID)"

family_of() {
    case "$1" in
        debian|ubuntu)                                        echo debian ;;
        fedora|rhel|centos|almalinux|rocky)                   echo fedora ;;
        opensuse|opensuse-leap|opensuse-tumbleweed|sles|suse) echo suse ;;
        arch|archarm|cachyos|manjaro|endeavouros)             echo arch ;;
        alpine)                                               echo alpine ;;
    esac
}
FAMILY="$(family_of "${OS_ID}")"
if [ -z "${FAMILY}" ]; then
    for _like in ${OS_ID_LIKE}; do
        FAMILY="$(family_of "${_like}")"
        [ -n "${FAMILY}" ] && break
    done
fi

# Which artifact is installed here (build-static.sh writes it beside the
# binary): `linux` brings its own CPython, `static` has none, and
# `debian13-python` is the older Debian 13 build that links the host's.
VARIANT=""
[ "${MODE}" = "static" ] && VARIANT="$(cat "${PREFIX}/VARIANT" 2>/dev/null || true)"

# ---------------------------------------------------------------------------
# Doing things (or, with --dry-run, saying what would be done)
# ---------------------------------------------------------------------------

log()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }

# Everything that changes the machine goes through `run`, which is what makes
# --dry-run trustworthy: it is the same code path with the doing taken out, not a
# second description of it that can drift.
run() {
    if [ "${DRY_RUN}" -eq 1 ]; then
        printf '    $ %s\n' "$*"
        return 0
    fi
    "$@"
}

# `${SUDO}` is deliberately unquoted where it is used: empty when this is already
# root, and one word when it is not.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null ||
        { echo "error: run this as root, or install sudo" >&2; exit 1; }
    SUDO="sudo"
fi

run_root() {
    if [ -n "${SUDO}" ]; then run "${SUDO}" "$@"; else run "$@"; fi
}

# The postgres superuser. `sudo -u` where there is a sudo, and `runuser` where
# this is already root — `${SUDO} -u postgres ...` would degenerate into running
# `-u` as a command the moment the script is run as root, which is how it arrives
# when it is piped into a root shell.
pg_run() {
    if [ -n "${SUDO}" ]; then run "${SUDO}" -u postgres "$@"; else run runuser -u postgres -- "$@"; fi
}

# The same, for a query whose *output* is wanted; never a dry run, since the
# answer decides whether anything is created.
pg_query() {
    if [ -n "${SUDO}" ]; then
        ${SUDO} -u postgres psql -tAc "$1"
    else
        runuser -u postgres -- psql -tAc "$1"
    fi
}

# Content on stdin, because these are heredocs at the call site.
write_root_file() {
    _path="$1"; _mode="$2"; _owner="${3:-}"
    if [ "${DRY_RUN}" -eq 1 ]; then
        printf '    write %s (mode %s%s):\n' "${_path}" "${_mode}" \
            "${_owner:+, owner ${_owner}}"
        sed 's/^/      | /'
        return 0
    fi
    ${SUDO} install -d -m 0755 "$(dirname "${_path}")"
    # Empty, at its final mode and owner, *before* the content goes in: `tee`
    # would otherwise create it at the umask's 0644 and leave a database
    # password world-readable for as long as the chmod takes to follow.
    ${SUDO} install -m "${_mode}" /dev/null "${_path}"
    [ -n "${_owner}" ] && ${SUDO} chown "${_owner}" "${_path}"
    ${SUDO} tee "${_path}" >/dev/null
    return 0
}

# A file that exists is a decision someone made — an edited unit, a config with a
# password in it — so a re-run keeps it unless --force says otherwise.
keep_existing() {
    [ "${FORCE}" -eq 0 ] || return 1
    [ -e "$1" ] || return 1
    note "keeping the existing $1 (--force to replace it)"
    return 0
}

# ---------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------

case "${FAMILY}" in
    debian) PKG_TOOL="apt-get" ;;
    fedora) PKG_TOOL="dnf" ;;
    suse)   PKG_TOOL="zypper" ;;
    arch)   PKG_TOOL="pacman" ;;
    alpine)
        cat >&2 <<EOF
error: this is Alpine, which runs OpenRC rather than systemd, and this script
  writes a systemd unit. On Alpine the release's install.sh is the whole install
  (the binary runs on its bundled glibc there); run
  ${PREFIX}/bin/feldspar serve from a service of your own.
EOF
        exit 1
        ;;
    *)
        EXTRA=""
        [ "${MODE}" = "source" ] && EXTRA=", a C toolchain, pkg-config, git and libclang"
        cat >&2 <<EOF
error: this script knows Debian, Ubuntu, Fedora, RHEL, openSUSE and Arch (and what
  derives from them), and this is '${OS_ID:-unknown}'.

  On another distribution install the equivalents of: postgresql, ca-certificates,
  curl${EXTRA}, and Node.js with an npm of 9.3.0 or newer — an older npm cannot
  install a module (§1b below) — and then follow README §2.3 onwards, which is
  what the rest of this does.
EOF
        exit 1
        ;;
esac
command -v "${PKG_TOOL}" >/dev/null ||
    { echo "error: this looks like ${OS_ID:-a ${FAMILY} system}, but there is no ${PKG_TOOL} here" >&2; exit 1; }
command -v systemctl >/dev/null ||
    { echo "error: no systemctl here; this script installs a systemd unit" >&2; exit 1; }

BINARY="${PREFIX}/bin/feldspar"
[ "${MODE}" = "static" ] || BINARY="/usr/local/bin/feldspar"

log "feldspar host setup: ${MODE} install on ${OS_ID:-unknown} (${FAMILY}), service user ${SERVICE_USER}, binary ${BINARY}"
[ "${DRY_RUN}" -eq 1 ] && log "dry run: nothing below is executed"
[ -n "${DOMAIN}" ] || note "no --domain: only the admin UI will be served (README §7)"

# ---------------------------------------------------------------------------
# 1. Packages
# ---------------------------------------------------------------------------

# Node.js is **not** in this list, and that is the point of §1b below: it comes
# from NodeSource rather than from the distribution, because Debian's and
# Ubuntu's npm is too old to install a module. `curl` and `ca-certificates` are
# here partly to fetch its signing key.
#
# The package table: the same roles under each family's names.
#
#              certificates, curl     PostgreSQL server + client      build (--from-source)
#   debian     ca-certificates curl   postgresql postgresql-client    build-essential pkg-config git libclang-dev
#   fedora     ca-certificates curl   postgresql-server postgresql    gcc gcc-c++ make pkgconf-pkg-config git clang-devel
#   suse       ca-certificates        postgresql-server postgresql    gcc gcc-c++ make pkg-config git clang-devel
#              ca-certificates-mozilla curl
#   arch       ca-certificates curl   postgresql                      base-devel pkgconf git clang
#
# libclang is a build-time requirement of the module runtime (deno_runtime
# reaches bindgen through rusqlite); the binary it produces does not use it.
case "${FAMILY}" in
    debian)
        PACKAGES="ca-certificates curl"
        PG_PACKAGES="postgresql postgresql-client"
        BUILD_PACKAGES="build-essential pkg-config git libclang-dev"
        ;;
    fedora)
        PACKAGES="ca-certificates curl"
        PG_PACKAGES="postgresql-server postgresql"
        BUILD_PACKAGES="gcc gcc-c++ make pkgconf-pkg-config git clang-devel"
        ;;
    suse)
        # ca-certificates is the tooling; the certificates are -mozilla.
        PACKAGES="ca-certificates ca-certificates-mozilla curl"
        PG_PACKAGES="postgresql-server postgresql"
        BUILD_PACKAGES="gcc gcc-c++ make pkg-config git clang-devel"
        ;;
    arch)
        PACKAGES="ca-certificates curl"
        PG_PACKAGES="postgresql"
        BUILD_PACKAGES="base-devel pkgconf git clang"
        ;;
esac
[ -n "${DATABASE_URL}" ] || PACKAGES="${PACKAGES} ${PG_PACKAGES}"
[ "${MODE}" = "source" ] && PACKAGES="${PACKAGES} ${BUILD_PACKAGES}"
# The older Debian 13 + Python artifact says so in ${PREFIX}/VARIANT. Its binary
# links Debian's libpython3.13, and installing a Python module wants an
# interpreter of the same version with the venv module, which Debian packages
# separately. The `linux` artifact needs none of it.
if [ "${VARIANT}" = "debian13-python" ] && [ "${FAMILY}" = debian ]; then
    PACKAGES="${PACKAGES} libpython3.13 python3 python3-venv"
fi

# The family's package manager, non-interactively. Arch updates its package
# database only with a full upgrade, which it requires of any install.
pkg_refresh() {
    case "${FAMILY}" in
        debian) run_root apt-get update ;;
        fedora|suse|arch) ;;
    esac
}
pkg_install() {
    case "${FAMILY}" in
        debian) run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" ;;
        fedora) run_root dnf install -y "$@" ;;
        suse)   run_root zypper --non-interactive install "$@" ;;
        arch)   run_root pacman -Syu --needed --noconfirm "$@" ;;
    esac
}

log "installing packages: ${PACKAGES}"
pkg_refresh
# shellcheck disable=SC2086  # a word list, deliberately split
pkg_install ${PACKAGES}

# PostgreSQL's first start. Debian's package creates a cluster and starts it;
# the others leave that to the administrator. Each step is skipped where its
# work is already done, so a re-run changes nothing. Arch's initdb gets peer
# authentication on the socket explicitly, because its default is `trust`.
if [ -z "${DATABASE_URL}" ]; then
    case "${FAMILY}" in
        fedora)
            if [ -f /var/lib/pgsql/data/PG_VERSION ]; then
                note "PostgreSQL's data directory is already initialised"
            else
                log "initialising PostgreSQL's data directory"
                run_root postgresql-setup --initdb
            fi
            ;;
        arch)
            if [ -f /var/lib/postgres/data/PG_VERSION ]; then
                note "PostgreSQL's data directory is already initialised"
            else
                log "initialising PostgreSQL's data directory"
                pg_run initdb --locale=C.UTF-8 --encoding=UTF8 --auth-local=peer \
                    --auth-host=scram-sha-256 -D /var/lib/postgres/data
            fi
            ;;
    esac
    case "${FAMILY}" in
        fedora|suse|arch)
            log "enabling and starting postgresql.service"
            run_root systemctl enable --now postgresql
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# 1b. Node.js, from NodeSource
# ---------------------------------------------------------------------------
#
# npm is a **run-time** dependency in both modes and not a build one: the server
# shells out to it whenever an application is built or a module installed, from
# the admin UI's Build button as much as from the command line.
#
# It comes from NodeSource rather than from `apt install nodejs npm`, and that
# is a correctness requirement rather than a preference for something newer.
# Debian 12, Debian 13 and Ubuntu 24.04 all package **npm 9.2.0**, which cannot install a
# module at all: the modules directory depends on the v1 API stub packages at a
# `file:` path *and* overrides the same names, and npm before 9.3.0 hands that
# path to semver, so every install — of any module, whatever it depends on —
# dies with `Invalid comparator: file:/…/v1-api-stub/saltcorn-data`. npm 9.3.0
# (arborist 6.1.6) parses the specifier before comparing it, and is the floor
# `sc-module`'s installer enforces.
#
# A machine that already has a new enough npm keeps it: an operator who
# installed Node themselves — nvm, fnm, a newer distribution, a corporate
# mirror — has made a decision, and this script is not the place to overrule it.
NODE_MAJOR=26
MIN_NPM="9.3.0"
NODE_KEYRING="/etc/apt/keyrings/nodesource.asc"
NODE_LIST="/etc/apt/sources.list.d/nodesource.list"
NODE_PIN="/etc/apt/preferences.d/nodesource"

# Whether the npm already on this machine is one the server can install with:
# ${MIN_NPM} or newer. Never a dry run — the answer decides what is installed,
# and asking a program its version changes nothing.
npm_new_enough() {
    command -v npm >/dev/null 2>&1 || return 1
    _npm="$(npm --version 2>/dev/null)" || return 1
    _major="${_npm%%.*}"
    _rest="${_npm#*.}"
    _minor="${_rest%%.*}"
    case "${_major}:${_minor}" in
        ''|*[!0-9]*:*|*:*[!0-9]*) return 1 ;;   # not two numbers: assume not
    esac
    if [ "${_major}" -gt 9 ]; then return 0; fi
    if [ "${_major}" -eq 9 ] && [ "${_minor}" -ge 3 ]; then return 0; fi
    return 1
}

if npm_new_enough; then
    note "npm $(npm --version) is already here and new enough (${MIN_NPM}+); leaving Node alone"
else
    if command -v npm >/dev/null 2>&1; then
        note "npm $(npm --version) is too old to install a module (${MIN_NPM}+ is needed)"
    fi
    case "${FAMILY}" in
    debian)
        log "installing Node.js ${NODE_MAJOR}.x from NodeSource"
        run_root install -d -m 0755 /etc/apt/keyrings
        # The armoured key straight to a file rather than through `gpg --dearmor`:
        # apt reads an ASCII-armoured keyring given a `.asc` name, which is one
        # fewer package to install and one fewer pipeline to get right under sudo.
        run_root curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
            -o "${NODE_KEYRING}"
        run_root chmod 0644 "${NODE_KEYRING}"
        printf 'deb [signed-by=%s] https://deb.nodesource.com/node_%s.x nodistro main\n' \
            "${NODE_KEYRING}" "${NODE_MAJOR}" | write_root_file "${NODE_LIST}" 0644
        # Pinned, so that a distribution which later ships a `nodejs` of its own
        # with a higher version string cannot quietly replace this one and put the
        # host back where it started.
        printf 'Package: nodejs\nPin: origin deb.nodesource.com\nPin-Priority: 600\n' |
            write_root_file "${NODE_PIN}" 0644
        run_root apt-get update
        # NodeSource's `nodejs` carries its own npm and Conflicts/Provides the
        # distribution's `npm` package, which is why that is not in ${PACKAGES} —
        # and why apt removes a distribution npm already installed here rather than
        # ending up with two.
        run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs
        ;;
    *)
        # The other families package a Node whose npm is new enough — Fedora
        # and RHEL 10 (npm 10), openSUSE (versioned names on Leap), Arch — so
        # theirs is used, and checked once it is there.
        case "${FAMILY}:${OS_ID}" in
            suse:opensuse-tumbleweed) NODE_PACKAGES="nodejs-default npm-default" ;;
            suse:*)                   NODE_PACKAGES="nodejs22 npm22" ;;
            *)                        NODE_PACKAGES="nodejs npm" ;;
        esac
        log "installing Node.js from ${OS_ID:-the distribution}'s packages: ${NODE_PACKAGES}"
        # shellcheck disable=SC2086  # a word list, deliberately split
        pkg_install ${NODE_PACKAGES}
        if [ "${DRY_RUN}" -eq 0 ] && ! npm_new_enough; then
            note "the npm that installed is $(npm --version 2>/dev/null || echo missing), and modules need ${MIN_NPM}+;"
            note "install a newer Node (nodejs.org, nvm) before installing a module"
        fi
        ;;
    esac
fi

# ---------------------------------------------------------------------------
# 1c. A headless browser, for the coding agent's view_app
# ---------------------------------------------------------------------------
#
# The server finds it by the `browser` setting in the configuration file, or on
# PATH as chromium, chromium-browser or google-chrome — skipping a snap shim,
# for the reason in the header. What this installs is found on PATH, so the
# configuration file needs nothing.

# The first browser on PATH that is a real binary rather than a snap shim, or
# nothing. Never a dry run: the answer decides what is installed.
find_browser() {
    for _name in chromium google-chrome google-chrome-stable chromium-browser; do
        _path="$(command -v "${_name}" 2>/dev/null)" || continue
        _real="$(readlink -f "${_path}" 2>/dev/null || echo "${_path}")"
        # /snap/bin/chromium is a symlink to /usr/bin/snap itself.
        case "${_path}:${_real}" in /snap/*|*:/snap/*|*/bin/snap) continue ;; esac
        # Ubuntu's /usr/bin/chromium-browser is a shell script that execs the snap.
        if head -c 2 "${_real}" 2>/dev/null | grep -q '#!' &&
            grep -q '/snap/' "${_real}" 2>/dev/null; then
            continue
        fi
        echo "${_path}"
        return 0
    done
    return 1
}

BROWSER_PATH=""
if [ "${BROWSER}" -eq 0 ]; then
    note "--no-browser: view_app will be unavailable"
elif BROWSER_PATH="$(find_browser)"; then
    note "a headless-capable browser is already here: ${BROWSER_PATH}"
else
    BROWSER_PATH=""
    case "${FAMILY}:${OS_ID}" in
        debian:ubuntu)
            ARCH="$(dpkg --print-architecture 2>/dev/null || echo unknown)"
            if [ "${ARCH}" = "amd64" ]; then
                log "installing Google Chrome from Google's apt repository (Ubuntu's chromium-browser is a snap)"
                CHROME_KEYRING="/etc/apt/keyrings/google-chrome.asc"
                run_root install -d -m 0755 /etc/apt/keyrings
                run_root curl -fsSL https://dl.google.com/linux/linux_signing_key.pub \
                    -o "${CHROME_KEYRING}"
                run_root chmod 0644 "${CHROME_KEYRING}"
                printf 'deb [arch=amd64 signed-by=%s] https://dl.google.com/linux/chrome/deb/ stable main\n' \
                    "${CHROME_KEYRING}" | write_root_file /etc/apt/sources.list.d/google-chrome.list 0644
                run_root apt-get update
                run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y google-chrome-stable
                BROWSER_PATH="/usr/bin/google-chrome"
            else
                note "no non-snap Chromium is known for Ubuntu on ${ARCH}: view_app will be"
                note "unavailable until one is installed and named with \`browser\` in ${CONFIG_FILE}"
            fi
            ;;
        debian:*)
            # Debian 12 and 13, and anything else apt-based that packages it.
            log "installing chromium"
            pkg_install chromium
            BROWSER_PATH="/usr/bin/chromium"
            ;;
        fedora:fedora)
            log "installing chromium"
            pkg_install chromium
            BROWSER_PATH="/usr/bin/chromium-browser"
            ;;
        fedora:*)
            # RHEL and its rebuilds package Chromium in EPEL. Alma and Rocky
            # carry `epel-release` in their own extras; RHEL itself fetches it
            # from the Fedora project.
            log "installing chromium from EPEL"
            if [ "${OS_ID}" = rhel ]; then
                pkg_install "https://dl.fedoraproject.org/pub/epel/epel-release-latest-${OS_VERSION_ID%%.*}.noarch.rpm"
            else
                pkg_install epel-release
            fi
            pkg_install chromium
            BROWSER_PATH="/usr/bin/chromium-browser"
            ;;
        suse:*|arch:*)
            log "installing chromium"
            pkg_install chromium
            BROWSER_PATH="/usr/bin/chromium"
            ;;
    esac
    # Where the package put it, when that is not where it is on most hosts.
    if [ "${DRY_RUN}" -eq 0 ] && [ -n "${BROWSER_PATH}" ] && [ ! -x "${BROWSER_PATH}" ]; then
        BROWSER_PATH="$(find_browser || true)"
    fi
fi

# ---------------------------------------------------------------------------
# 2. The service account
# ---------------------------------------------------------------------------

if id "${SERVICE_USER}" >/dev/null 2>&1; then
    note "the ${SERVICE_USER} account already exists"
else
    log "creating the ${SERVICE_USER} system account"
    # The home is the *state* directory, never the program tree: a service
    # account owns what it writes and nothing it executes. ${PREFIX} stays
    # root-owned and read-only to the service, which is what stops a compromised
    # server from rewriting its own binary and surviving a restart.
    #
    # It has to agree with the unit's HOME (§6), or `sudo -u ${SERVICE_USER} ...`
    # from a shell lands somewhere the account cannot write: that is where git
    # looks for the identity `feldspar`'s commits are made under.
    #
    # Debian's adduser and everyone else's useradd say the same thing: a
    # system account with a group of its own name, that home, no login shell.
    if [ "${FAMILY}" = debian ]; then
        run_root adduser --system --group --home "${STATE_DIR}" "${SERVICE_USER}"
    else
        run_root useradd --system --user-group --create-home --home-dir "${STATE_DIR}" \
            --shell /usr/sbin/nologin "${SERVICE_USER}"
    fi
fi

# ---------------------------------------------------------------------------
# 2b. The browser runs as the service account
# ---------------------------------------------------------------------------
#
# Started once, the way the server will start it: headless, as ${SERVICE_USER},
# with a throwaway profile directory. A browser that cannot start here (a
# missing library, a sandbox the kernel refuses) is reported now, with its own
# error, rather than as a failed view_app later.

if [ -n "${BROWSER_PATH}" ]; then
    log "checking that ${BROWSER_PATH} runs headless as ${SERVICE_USER}"
    if [ "${DRY_RUN}" -eq 1 ]; then
        run_root runuser -u "${SERVICE_USER}" -- "${BROWSER_PATH}" --headless \
            --user-data-dir=/tmp/feldspar-browser-check --dump-dom about:blank
    else
        # Reported, never fatal: a browser that does not start leaves view_app
        # unavailable, and everything else this script sets up still works.
        _as_service() { ${SUDO} runuser -u "${SERVICE_USER}" -- "$@"; }
        _log="$(mktemp)"
        if ! _profile="$(_as_service mktemp -d /tmp/feldspar-browser-XXXXXX 2>"${_log}")"; then
            note "could not run anything as ${SERVICE_USER} to check it:"
            sed 's/^/      | /' "${_log}"
        elif _as_service timeout 60 "${BROWSER_PATH}" --headless --user-data-dir="${_profile}" \
            --dump-dom about:blank 2>"${_log}" | grep -q '<html'; then
            note "it does"
        elif _as_service timeout 60 "${BROWSER_PATH}" --headless --no-sandbox \
            --user-data-dir="${_profile}" --dump-dom about:blank 2>/dev/null | grep -q '<html'; then
            note "it runs only without its sandbox on this kernel; to let the server do the"
            note "same, add  browser_sandbox = false  to the environment in ${CONFIG_FILE}"
        else
            note "it does not; view_app will fail until this works:"
            tail -n 20 "${_log}" | sed 's/^/      | /'
        fi
        [ -n "${_profile:-}" ] && ${SUDO} rm -rf "${_profile}"
        rm -f "${_log}"
    fi
fi

# ---------------------------------------------------------------------------
# 3. The database
# ---------------------------------------------------------------------------

if [ -n "${DATABASE_URL}" ]; then
    log "using the database at the given --database-url; creating nothing here"
else
    log "creating the ${DB_USER} role and the ${DB_NAME} database"
    # Peer authentication over the Unix socket: the role has the name of the
    # system account, so the service connects as itself and no database password
    # is written to disk anywhere.
    if [ "${DRY_RUN}" -eq 1 ]; then
        pg_run createuser "${DB_USER}"
        pg_run createdb -O "${DB_USER}" "${DB_NAME}"
    else
        if pg_query "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" | grep -q 1; then
            note "the ${DB_USER} role already exists"
        else
            pg_run createuser "${DB_USER}"
        fi
        if pg_query "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" | grep -q 1; then
            note "the ${DB_NAME} database already exists"
        else
            pg_run createdb -O "${DB_USER}" "${DB_NAME}"
        fi
    fi
fi

# ---------------------------------------------------------------------------
# 4. The binary
# ---------------------------------------------------------------------------

if [ "${MODE}" = "source" ]; then
    # rustup goes in as the *invoking* user, not root and not the service
    # account: the toolchain is only needed to build, and the service never
    # touches it. Same for the checkout, which the service only ever reads.
    if command -v cargo >/dev/null || [ -x "${HOME}/.cargo/bin/cargo" ]; then
        note "cargo is already installed"
    else
        log "installing rustup (as $(id -un))"
        run sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
    fi
    [ -f "${HOME}/.cargo/env" ] && . "${HOME}/.cargo/env"

    if [ -d "${SRC_DIR}/.git" ]; then
        log "updating the checkout at ${SRC_DIR}"
        run git -C "${SRC_DIR}" fetch --quiet origin "${BRANCH}"
        run git -C "${SRC_DIR}" checkout --quiet "${BRANCH}"
        run git -C "${SRC_DIR}" pull --quiet --ff-only
    else
        log "cloning ${REPO} into ${SRC_DIR}"
        run_root install -d -o "$(id -un)" -g "$(id -gn)" "${SRC_DIR}"
        run git clone --branch "${BRANCH}" "${REPO}" "${SRC_DIR}"
    fi

    # The release build also runs `npm ci && npm run build` in ui/admin and
    # ui/ide, and records their absolute paths in the binary — which is why the
    # checkout has to stay where it is built.
    log "building (this takes a while and wants a few GB of RAM)"
    run sh -c "cd '${SRC_DIR}' && cargo build --release -p sc-cli"
    run_root install -m 0755 "${SRC_DIR}/target/release/feldspar" "${BINARY}"
else
    # The other half of the two-step install. Either order works, so a missing
    # binary here is not an error: the unit is enabled and left stopped.
    if [ -x "${BINARY}" ]; then
        note "found the deployed binary at ${BINARY}"
    else
        note "no binary at ${BINARY} yet — unpack a release tarball and run its install.sh"
        START=0
    fi
    # On PATH under its own name, for `feldspar build-app` and friends. A symlink
    # rather than a copy, so a later deploy is picked up with nothing to re-run.
    run_root ln -sfn "${BINARY}" /usr/local/bin/feldspar
fi

# ---------------------------------------------------------------------------
# 4b. Certificates for the embedded Python
# ---------------------------------------------------------------------------
#
# The `linux` artifact's CPython (python-build-standalone) has its OpenSSL's
# certificate path compiled in, and whether a distribution keeps its bundle
# there is that distribution's answer (TODO-portable-release D10). So the
# answer is asked, not assumed: the self-check's verified HTTPS request, as the
# service account. Where it fails and passes with the distribution's bundle in
# SSL_CERT_FILE, the unit sets that. Children inherit it, which is harmless:
# it names the right file. pip is not the reason — it brings its own
# certificates — Python code calling `ssl` or `urllib` is.

SSL_CERT_SETTING=""
if [ "${VARIANT}" = "linux" ] && { [ -x "${BINARY}" ] || [ "${DRY_RUN}" -eq 1 ]; }; then
    log "checking that the embedded Python makes a verified HTTPS request as ${SERVICE_USER}"
    if [ "${DRY_RUN}" -eq 1 ]; then
        run_root runuser -u "${SERVICE_USER}" -- "${BINARY}" python check
    else
        # The unit's environment, not this shell's: a caller's SSL_CERT_FILE
        # (cargo sets one for everything it runs) would make the first check
        # pass for a reason the service will not have.
        _check() { ${SUDO} runuser -u "${SERVICE_USER}" -- env -u SSL_CERT_FILE -u SSL_CERT_DIR "$@" "${BINARY}" python check; }
        _log="$(mktemp)"
        if _check >"${_log}" 2>&1; then
            note "it does"
        else
            for _bundle in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
                           /etc/ssl/ca-bundle.pem; do
                [ -f "${_bundle}" ] || continue
                if _check "SSL_CERT_FILE=${_bundle}" >/dev/null 2>&1; then
                    SSL_CERT_SETTING="Environment=SSL_CERT_FILE=${_bundle}"
                    note "only with SSL_CERT_FILE=${_bundle}; the unit sets it"
                    break
                fi
            done
            if [ -z "${SSL_CERT_SETTING}" ]; then
                note "it does not, with this host's certificate bundle or without it. Python code"
                note "that makes HTTPS requests will fail until this works (no network here?):"
                tail -n 5 "${_log}" | sed 's/^/      | /'
            fi
        fi
        rm -f "${_log}"
    fi
fi

# ---------------------------------------------------------------------------
# 5. The configuration file
# ---------------------------------------------------------------------------

# A service account has no home directory to search, so the file goes in the
# system location — and the unit names it outright with FELDSPAR_CONFIG.
if keep_existing "${CONFIG_FILE}"; then
    :
else
    log "writing ${CONFIG_FILE}"
    if [ -n "${DATABASE_URL}" ]; then
        DB_SETTINGS="url = \"${DATABASE_URL}\""
    else
        DB_SETTINGS="host = \"/var/run/postgresql\"   # a leading \`/\` is a Unix socket directory
user = \"${DB_USER}\"
database = \"${DB_NAME}\""
    fi
    if [ -n "${DOMAIN}" ]; then
        DOMAIN_SETTING="base_domain = \"${DOMAIN}\""
    else
        DOMAIN_SETTING="# base_domain = \"example.com\"   # uncomment to serve applications"
    fi
    # root:${SERVICE_USER} 0640 — the service *reads* its configuration and never
    # writes it, so it does not own it. A file like this may hold a database
    # password, which a compromised server can read either way; what the
    # ownership buys is that it cannot rewrite the file to point the next restart
    # somewhere else. Nobody outside the group can read it at all, and the server
    # warns on stderr about modes wider than this.
    write_root_file "${CONFIG_FILE}" 0640 "root:${SERVICE_USER}" <<EOF
# Written by scripts/setup-host.sh. See README §7 for everything that can go here.
default_environment = "${ENVIRONMENT}"

[environments.${ENVIRONMENT}]
${DB_SETTINGS}

${DOMAIN_SETTING}
bind = "${BIND}"
EOF
fi

# ---------------------------------------------------------------------------
# 6. The systemd unit
# ---------------------------------------------------------------------------

if keep_existing "${UNIT_FILE}"; then
    if [ -n "${SSL_CERT_SETTING}" ] && ! grep -q '^Environment=SSL_CERT_FILE=' "${UNIT_FILE}" 2>/dev/null; then
        note "add this line to [Service] in ${UNIT_FILE}, for the reason in §4b above:"
        note "  ${SSL_CERT_SETTING}"
    fi
else
    log "writing ${UNIT_FILE}"
    # The certificate bundle for the embedded Python, where §4b found it needed.
    UNIT_ENV="Environment=FELDSPAR_CONFIG=${CONFIG_FILE}
Environment=HOME=${STATE_DIR}
Environment=SC_DATA_DIR=${STATE_DIR}"
    [ -z "${SSL_CERT_SETTING}" ] || UNIT_ENV="${UNIT_ENV}
${SSL_CERT_SETTING}"
    # Type=notify, so `systemctl start` returns when the port is accepting rather
    # than when the process exists; the watchdog restarts a runtime that has
    # stopped scheduling; --environment is named even though it is the file's
    # default, so an ambient DATABASE_URL cannot redirect the service; and
    # StateDirectory is the one writable path under ProtectSystem=strict, which
    # is where file stores and npm's cache have to live.
    #
    # SC_DATA_DIR names that same directory outright, which is what makes the
    # admin UI able to *suggest* a place for a new file store: git checkouts and
    # the local-store directories the "Suggest a directory" button offers both
    # land under it. Without it the server would derive one from HOME
    # (/var/lib/feldspar/.local/share/feldspar) — writable, but a place no
    # operator would think to look.
    write_root_file "${UNIT_FILE}" 0644 <<EOF
[Unit]
Description=Saltcorn Feldspar
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=notify
WatchdogSec=30s
User=${SERVICE_USER}
Group=${SERVICE_USER}
ExecStart=${BINARY} serve --environment ${ENVIRONMENT}
${UNIT_ENV}
StateDirectory=feldspar
WorkingDirectory=${STATE_DIR}
Restart=on-failure
RestartSec=5s

# Bind ports 80/443 without running as root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# Everything read-only except the state directory. In particular ${PREFIX} and
# the binary are root's: the server runs user-authored JavaScript and Python and
# shells out to git, so the account must be assumed reachable, and an account
# that cannot write the code it executes cannot survive a restart.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=${STATE_DIR}

# The rest of the kernel surface. Deliberately *not* here:
#   MemoryDenyWriteExecute  breaks the V8 and Python JITs the module runtime is.
#   SystemCallFilter        blocks clone(CLONE_NEWUSER), which is Chromium's
#   RestrictNamespaces      sandbox; view_app would need --no-sandbox, and a
#                           browser rendering application content is the last
#                           place to give that up.
#   ProcSubset=pid          hides /proc/cpuinfo and /proc/meminfo, which V8 sizes
#                           its heap from and Chromium reads at startup.
# AF_NETLINK is in the address families because glibc's getaddrinfo asks it which
# addresses are configured before it resolves anything.
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
fi

UNIT_NAME="$(basename "${UNIT_FILE}")"

run_root systemctl daemon-reload
if [ "${START}" -eq 1 ]; then
    log "enabling and starting ${UNIT_NAME}"
    run_root systemctl enable --now "${UNIT_NAME}"
else
    log "enabling ${UNIT_NAME} (not starting it)"
    run_root systemctl enable "${UNIT_NAME}"
fi

# ---------------------------------------------------------------------------
# What is left for a human
# ---------------------------------------------------------------------------

PORT="${BIND##*:}"
printf '\n'
if [ "${DRY_RUN}" -eq 1 ]; then
    log "dry run finished; nothing above was executed"
    exit 0
fi

if [ "${START}" -eq 1 ]; then
    if systemctl is-active --quiet "${UNIT_NAME}"; then
        log "feldspar is running"
        note "curl -s http://127.0.0.1:${PORT}/health"
    else
        log "${UNIT_NAME} did not come up; its reason is in the log:"
        note "journalctl -u ${UNIT_NAME} -n 50 --no-pager"
        exit 1
    fi
else
    log "the unit is enabled but not running"
    if [ "${MODE}" = "static" ] && [ ! -x "${BINARY}" ]; then
        note "put a binary there: unpack a release tarball and run its install.sh,"
        note "or from a checkout on your workstation:"
        note "  scripts/build-static.sh --deploy $(id -un)@$(uname -n)"
    fi
    note "then: ${SUDO:+sudo }systemctl start ${UNIT_NAME}"
fi

cat <<EOF

Still to do, and none of it is this script's business (README §2.7):
  DNS      point ${DOMAIN:-your domain} and *.${DOMAIN:-your domain} at this host
  Admin    open http://${DOMAIN:-this host}:${PORT}/ and create the first user —
           that screen is open to whoever reaches it first, so do it now
  TLS      admin UI, Settings -> SSL / TLS certificates (not a command-line flag)
  Firewall only ${PORT} and 443 need to be open; Postgres stays on its socket
EOF
