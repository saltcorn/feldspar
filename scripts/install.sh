#!/bin/sh
#
# Install the `feldspar` executable — and only that — from the latest
# release, or upgrade the one that is installed:
#
#     curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh
#     wget -qO-  https://feldspar-latest-static.saltcorn.com/install.sh | sh
#
# It is the first of two steps. This one puts the release tree at /opt/feldspar
# (the binary, and the admin UI bundles whose paths are compiled into it) and
# links `feldspar` onto PATH; the second turns the machine into an instance:
#
#     feldspar setup --dev          # SQLite, `feldspar serve` started by hand
#     sudo feldspar setup --server  # PostgreSQL here, a system service
#     feldspar setup                # asks
#
# Which release: `--variant static` (no Python, a static binary) or `--variant
# linux` (with its own CPython, and its own glibc for Alpine). An upgrade
# fetches the one /opt/feldspar/VARIANT names, so a host keeps what it has;
# `debian13-python` (the Python build before `linux`) means `linux`. A new
# install gets `static` unless told otherwise.
#
# Running it again is the upgrade. The tarball's own install.sh does the
# install (scripts/lib/install-tree.sh): it copies the tree beside
# /opt/feldspar and swaps it in by rename, so a running server is never
# written over; it stops running feldspar.service and feldspar@*.service units
# for the swap and starts them again (`--no-restart` leaves them alone); and it
# keeps the previous tree as /opt/feldspar.old, printing the command that puts
# it back.
#
# What it installs beside the binary is what this script needs to fetch and
# unpack the release and what the binary needs to *run*: ca-certificates (the
# server makes outbound TLS connections — ACME, LLM providers, SMTP), a
# downloader, tar, gzip, find and awk. PostgreSQL, Node.js and a browser are
# `feldspar setup`'s business, or scripts/setup-host.sh's.
#
# Debian, Ubuntu, Fedora, RHEL (Alma, Rocky), openSUSE, Arch and Alpine on
# x86_64, and what /etc/os-release says derives from them (ID_LIKE): Mint, MX,
# CachyOS, Manjaro, EndeavourOS. Anything else is refused by name rather than
# half-installed; `--force` tries anyway, without the package manager.
#
# Plain POSIX sh, not bash: Alpine's /bin/sh is busybox ash, and bash is one of
# the things it does not have. Everything after `sh -s --` is an option of this
# script; `--help` lists them, and `--dry-run` prints the commands without
# running any.
#
set -eu

BASE_URL="${FELDSPAR_BASE_URL:-https://feldspar-latest-static.saltcorn.com}"
# Fixed by the build: the binary carries ${PREFIX}/ui/... as where its bundles
# are, so the tree cannot go anywhere else (build-static.sh --prefix).
PREFIX="/opt/feldspar"
LINK="/usr/local/bin/feldspar"
# The glibc the linux artifact's normal binary needs (build-static.sh's
# BUILD_GLIBC); on an older one, or on musl, it runs on its bundled glibc.
BUILD_GLIBC="2.36"
# Read in place of /etc/os-release when set: how the tests stand in for
# another distribution.
OS_RELEASE="${FELDSPAR_OS_RELEASE:-/etc/os-release}"
DRY_RUN=0
FORCE=0
SKIP_PACKAGES=0
VARIANT=""
RESTART=1

usage() {
    cat <<EOF
Install the feldspar executable from the latest release, or upgrade it.

Usage: curl -fsSL ${BASE_URL}/install.sh | sh -s -- [options]

      --variant V       static (no Python) or linux (with Python; on Alpine too).
                        Default: what ${PREFIX}/VARIANT names, else static.
      --base-url URL    Where to download the release from (default ${BASE_URL},
                        or \$FELDSPAR_BASE_URL). A file:// URL reads a directory.
      --no-restart      Do not stop running feldspar units for the upgrade or
                        start them after it; they keep the old release until
                        they are restarted.
      --no-packages     Do not install ca-certificates, curl/wget, tar, gzip,
                        find or awk with the package manager; fail instead if
                        one is missing.
      --force           Install on a distribution or architecture this script
                        does not recognise.
  -n, --dry-run         Print what would be done, do nothing.
  -h, --help            This message.

Then: feldspar setup --dev, or sudo feldspar setup --server.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --variant)
            [ $# -ge 2 ] || { echo "error: --variant needs a value" >&2; exit 2; }
            VARIANT="$2"; shift 2 ;;
        --base-url)
            [ $# -ge 2 ] || { echo "error: --base-url needs a value" >&2; exit 2; }
            BASE_URL="$2"; shift 2 ;;
        --no-restart)  RESTART=0; shift ;;
        --no-packages) SKIP_PACKAGES=1; shift ;;
        --force)       FORCE=1; shift ;;
        -n|--dry-run)  DRY_RUN=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *)             echo "error: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done
BASE_URL="${BASE_URL%/}"

log()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# Everything that changes the machine goes through here, so that --dry-run is
# the same code path with the doing taken out.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    elif command -v doas >/dev/null 2>&1; then
        SUDO="doas"
    elif [ "${DRY_RUN}" -eq 0 ]; then
        die "${PREFIX} needs root: run this as root, or install sudo"
    fi
fi
run_root() {
    if [ "${DRY_RUN}" -eq 1 ]; then
        printf '    $ %s\n' "${SUDO:+${SUDO} }$*"
        return 0
    fi
    ${SUDO} "$@"
}

# ---------------------------------------------------------------------------
# Which release
# ---------------------------------------------------------------------------

INSTALLED="$(cat "${PREFIX}/VARIANT" 2>/dev/null || true)"
[ "${INSTALLED}" = debian13-python ] && INSTALLED="linux"
if [ -z "${VARIANT}" ]; then
    VARIANT="${INSTALLED:-static}"
fi
case "${VARIANT}" in
    static) OBJECT="feldspar.tar.gz" ;;
    # Published under the Debian 13 build's name, which hosts already fetch,
    # until the names carry the platform (TODO-portable-release D5, R3.1).
    linux|python|debian13-python) VARIANT="linux"; OBJECT="feldspar-debian13-python.tar.gz" ;;
    *) die "--variant must be static or linux, got ${VARIANT}" ;;
esac

# ---------------------------------------------------------------------------
# Is this a machine the release runs on?
# ---------------------------------------------------------------------------

ID=""
ID_LIKE=""
if [ -r "${OS_RELEASE}" ]; then
    # shellcheck disable=SC1090
    . "${OS_RELEASE}"
fi
# The package manager's family: by ID, else by the first family ID_LIKE names.
family_of() {
    case "$1" in
        debian|ubuntu)                                    echo debian ;;
        fedora|rhel|centos|almalinux|rocky)               echo fedora ;;
        opensuse|opensuse-leap|opensuse-tumbleweed|sles|suse) echo suse ;;
        arch|archarm|cachyos|manjaro|endeavouros)          echo arch ;;
        alpine)                                           echo alpine ;;
    esac
}
FAMILY="$(family_of "${ID}")"
if [ -z "${FAMILY}" ]; then
    for like in ${ID_LIKE}; do
        FAMILY="$(family_of "${like}")"
        [ -n "${FAMILY}" ] && break
    done
fi
if [ -z "${FAMILY}" ]; then
    [ "${FORCE}" -eq 1 ] ||
        die "this installer knows Debian, Ubuntu, Fedora, RHEL, openSUSE, Arch and Alpine
  (and what derives from them), and this is '${ID:-unknown}'. The release may well
  run here: --force installs it anyway, without the package manager (so
  ca-certificates, tar, gzip, find, awk and curl or wget must already be here)."
    SKIP_PACKAGES=1
fi

ARCH="$(uname -m)"
case "${ARCH}" in
    x86_64|amd64) ;;
    *)
        [ "${FORCE}" -eq 1 ] ||
            die "the release is built for x86_64, and this machine is ${ARCH}"
        ;;
esac

# What the linux artifact will run on here — the tarball's install.sh decides,
# by the same tests and by starting the binary; this is the forecast, for the
# log and for --dry-run.
libc_forecast() {
    if [ "${VARIANT}" != linux ]; then
        echo "none needed: the static binary links no libc"
        return 0
    fi
    if ldd --version 2>&1 | grep -qi musl; then
        echo "the bundled glibc (this is a musl host)"
        return 0
    fi
    _v="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
    _v="${_v#glibc }"
    case "${_v}" in
        "") echo "decided by starting the binary: this host's libc is unknown" ;;
        *)
            # Version order, numerically, field by field: is ${_v} >= ${BUILD_GLIBC}?
            if [ "$(printf '%s\n%s\n' "${BUILD_GLIBC}" "${_v}" | sort -t. -k1,1n -k2,2n | head -n 1)" = "${BUILD_GLIBC}" ]; then
                echo "the host's glibc ${_v}"
            else
                echo "the bundled glibc (the host's ${_v} is older than ${BUILD_GLIBC})"
            fi
            ;;
    esac
}

if [ -n "${INSTALLED}" ]; then
    log "upgrading feldspar (${INSTALLED}) from ${BASE_URL} (${ID:-unknown} ${ARCH})"
    [ "${INSTALLED}" = "${VARIANT}" ] || note "switching from the ${INSTALLED} release to ${VARIANT}"
else
    log "installing feldspar (${VARIANT}) from ${BASE_URL} (${ID:-unknown} ${ARCH})"
fi
note "libc: $(libc_forecast)"
[ "${DRY_RUN}" -eq 1 ] && log "dry run: nothing below is executed"

# ---------------------------------------------------------------------------
# What it takes to download, check and unpack — and to run
# ---------------------------------------------------------------------------

DOWNLOADER=""
if command -v curl >/dev/null 2>&1; then
    DOWNLOADER="curl"
elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER="wget"
fi

# Where each family keeps its certificate bundle: Debian, Arch and Alpine,
# then Fedora/RHEL, then openSUSE.
has_ca_bundle() {
    for _bundle in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
                   /etc/ssl/ca-bundle.pem; do
        [ -e "${_bundle}" ] && return 0
    done
    return 1
}

PACKAGES=""
if ! has_ca_bundle; then
    PACKAGES="ca-certificates"
    # openSUSE's ca-certificates is the tooling; the certificates are this.
    [ "${FAMILY}" = suse ] && PACKAGES="${PACKAGES} ca-certificates-mozilla"
fi
command -v tar >/dev/null 2>&1 || PACKAGES="${PACKAGES} tar"
command -v gzip >/dev/null 2>&1 || PACKAGES="${PACKAGES} gzip"
command -v find >/dev/null 2>&1 || PACKAGES="${PACKAGES} findutils"
command -v awk >/dev/null 2>&1 || PACKAGES="${PACKAGES} gawk"
# Alpine's busybox wget speaks TLS; Debian's minimal images have neither tool.
[ -n "${DOWNLOADER}" ] || { PACKAGES="${PACKAGES} curl"; DOWNLOADER="curl"; }
PACKAGES="${PACKAGES# }"

if [ -n "${PACKAGES}" ]; then
    if [ "${SKIP_PACKAGES}" -eq 1 ]; then
        die "missing: ${PACKAGES}; install them, or drop --no-packages"
    fi
    log "installing ${PACKAGES}"
    # shellcheck disable=SC2086  # a word list, deliberately split
    case "${FAMILY}" in
        debian)
            run_root apt-get update -qq
            run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ${PACKAGES}
            ;;
        fedora) run_root dnf install -y -q ${PACKAGES} ;;
        suse)   run_root zypper --non-interactive --quiet install ${PACKAGES} ;;
        # A full upgrade with the install: Arch does not support updating the
        # package database without one.
        arch)   run_root pacman -Syu --needed --noconfirm ${PACKAGES} ;;
        alpine) run_root apk add --no-cache ${PACKAGES} ;;
    esac
fi

fetch() {  # fetch URL FILE
    if [ "${DRY_RUN}" -eq 1 ]; then
        printf '    $ %s %s -> %s\n' "${DOWNLOADER}" "$1" "$2"
        return 0
    fi
    case "$1" in
        # A release directory on this machine or a mount: a mirror, or the
        # build's own smoke test (scripts/lib/smoke-linux.sh).
        file://*) cp "${1#file://}" "$2" ;;
        *)
            case "${DOWNLOADER}" in
                curl) curl -fsSL --proto-redir '=https' -o "$2" "$1" ;;
                wget) wget -qO "$2" "$1" ;;
            esac
            ;;
    esac || die "could not download $1"
}

# ---------------------------------------------------------------------------
# Download, check, unpack, install
# ---------------------------------------------------------------------------

TMP="$(mktemp -d "${TMPDIR:-/tmp}/feldspar-install.XXXXXX")"
trap 'rm -rf "${TMP}"' EXIT INT TERM

log "downloading ${OBJECT}"
fetch "${BASE_URL}/${OBJECT}" "${TMP}/${OBJECT}"
fetch "${BASE_URL}/${OBJECT}.sha256" "${TMP}/${OBJECT}.sha256"

if [ "${DRY_RUN}" -eq 0 ]; then
    # The checksum file names the published object, so it checks in place.
    # busybox's sha256sum takes -c as well as coreutils' does.
    (cd "${TMP}" && sha256sum -c "${OBJECT}.sha256" >/dev/null) ||
        die "the download does not match its checksum; try again"
    note "checksum ok"

    tar -xzf "${TMP}/${OBJECT}" -C "${TMP}"
    TREE=""
    for d in "${TMP}"/feldspar-*/; do
        [ -x "${d}install.sh" ] && TREE="${d%/}"
    done
    [ -n "${TREE}" ] || die "the tarball has no feldspar-*/install.sh in it"
else
    TREE="${TMP}/feldspar-<version>"
fi

log "installing to ${PREFIX}"
# The tarball's own installer: it knows the tree's layout, chooses the libc,
# swaps the tree in and restarts what was running — so the two install paths
# cannot drift apart. Its closing advice is about setup-host.sh; ours follows
# below.
if [ "${RESTART}" -eq 1 ]; then
    run_root "${TREE}/install.sh" --no-next-steps
else
    run_root "${TREE}/install.sh" --no-next-steps --no-restart
fi
run_root mkdir -p "$(dirname "${LINK}")"
run_root ln -sfn "${PREFIX}/bin/feldspar" "${LINK}"

if [ "${DRY_RUN}" -eq 1 ]; then
    log "dry run finished; nothing above was executed"
    exit 0
fi

log "installed ${PREFIX}/bin/feldspar, on PATH as ${LINK}"
if [ -n "${INSTALLED}" ]; then
    echo
    if [ "${RESTART}" -eq 1 ]; then
        echo "The upgrade is done: running feldspar units were stopped for the swap and started again."
    else
        echo "The upgrade is done. --no-restart left running feldspar processes on the old release"
        echo "until they are restarted."
    fi
    echo "Nothing else needs to run again: setup-host.sh and \`feldspar setup\` are for a new host."
    exit 0
fi
cat <<EOF

Next, make this machine an instance — either of:

  feldspar setup --dev          a development instance: SQLite, and you start
                                \`feldspar serve\` yourself
  sudo feldspar setup --server  a production server: PostgreSQL on this host, a
                                feldspar system account and a service started at boot
  feldspar setup                asks which, and which database

\`feldspar setup --help\` lists the options (a PostgreSQL elsewhere, a domain).
To upgrade later, run this installer again: it stops and starts running feldspar
units itself, and keeps the previous release as ${PREFIX}.old.
EOF
