#!/bin/sh
#
# Install the `feldspar` executable — and only that — from the latest
# release, or upgrade the one that is installed:
#
#     curl -fsSL https://feldspar-latest-static.saltcorn.com/install.sh | sh
#     wget -qO-  https://feldspar-latest-static.saltcorn.com/install.sh | sh
#
# It is the first of two steps. This one puts the release tree at /opt/feldspar
# (the binary, its own CPython and glibc, and the admin UI bundles whose paths
# are compiled into it) and links `feldspar` onto PATH; the second turns the
# machine into an instance:
#
#     sudo feldspar setup --server  # PostgreSQL here, a system service
#     feldspar setup --dev          # SQLite, `feldspar serve` started by hand
#     feldspar setup                # asks
#
# There is one release per architecture. A host on an older one — the static
# build, or the Debian 13 only `debian13-python` — gets it at its next upgrade.
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
# `feldspar setup`'s business.
#
# Debian, Ubuntu, Fedora, RHEL (Alma, Rocky), openSUSE, Arch and Alpine on
# x86_64, and what /etc/os-release says derives from them (ID_LIKE): Mint, MX,
# CachyOS, Manjaro, EndeavourOS. Another Linux distribution is refused by name
# rather than half-installed; `--force` tries anyway, without the package
# manager. A platform with no release (another architecture, macOS, Windows
# outside WSL2, the BSDs) is told what to do instead, and `--force` does not
# change that: there is nothing to download.
#
# Plain POSIX sh, not bash: Alpine's /bin/sh is busybox ash, and bash is one of
# the things it does not have. Everything after `sh -s --` is an option of this
# script — `sh -s` makes sh read the script from stdin and pass it the words
# after `--` as its arguments. None is needed; `--help` lists them, and
# `--dry-run` prints the commands without running any.
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
RESTART=1

usage() {
    cat <<EOF
Install the feldspar executable from the latest release, or upgrade it.

Usage: curl -fsSL ${BASE_URL}/install.sh | sh -s -- [options]

      --base-url URL    Where to download the release from (default ${BASE_URL},
                        or \$FELDSPAR_BASE_URL). A file:// URL reads a directory.
      --no-restart      Do not stop running feldspar units for the upgrade or
                        start them after it; they keep the old release until
                        they are restarted.
      --no-packages     Do not install ca-certificates, curl/wget, tar, gzip,
                        find or awk with the package manager; fail instead if
                        one is missing.
      --force           Install on a Linux distribution this script does not
                        recognise.
  -n, --dry-run         Print what would be done, do nothing.
  -h, --help            This message.

Then: feldspar setup --dev, or sudo feldspar setup --server.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        # Gone with the static release; accepted so that a command line
        # written for an older installer still works.
        --variant)
            [ $# -ge 2 ] || { echo "error: --variant needs a value" >&2; exit 2; }
            shift 2 ;;
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

# ---------------------------------------------------------------------------
# Is there a release for this platform?
# ---------------------------------------------------------------------------

# Checked first, before sudo or /etc/os-release, so that a Mac, a BSD or Git
# Bash on Windows hears what to do instead of failing at something else. There
# is no source-build fallback (TODO-portable-release D16): where there is no
# binary, a source build fails too, at V8, or needs more than a curl | sh
# should turn into. `--force` does not get past this: there is nothing to
# download.
SUPPORTED="Linux on x86_64 (Debian, Ubuntu, Fedora, RHEL, openSUSE, Arch, Alpine);
  Windows through WSL2"
OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}" in
    Linux)
        case "${ARCH}" in
            x86_64|amd64) ARCH="x86_64" ;;
            # Built by the same script, but not published yet
            # (TODO-portable-release R5).
            aarch64|arm64)
                die "there is no aarch64 Linux release yet; only x86_64 is published.
  An aarch64 machine can compile feldspar from a checkout and run it there
  (docs/OPERATIONS.md §2.3)." ;;
            armv*|armhf|arm)
                die "this is 32-bit ARM (${ARCH}), which has no release: V8 has no
  prebuilt build for it. Every Raspberry Pi from the 3 onward runs a 64-bit OS;
  install that, and run this again once aarch64 releases are published." ;;
            *)
                die "there is no release for Linux on ${ARCH}. Supported:
  ${SUPPORTED}" ;;
        esac
        ;;
    Darwin)
        case "${ARCH}" in
            arm64|aarch64)
                die "there is no macOS release yet. Build feldspar from source on the
  Mac: README.md §4.2." ;;
            *)
                die "there is no release for Intel Macs. Build feldspar from source:
  README.md §4.2." ;;
        esac
        ;;
    MINGW*|MSYS*|CYGWIN*)
        die "this is Windows (${OS}), which runs the Linux release under WSL2. In
  PowerShell: wsl --install -d Debian, then run this same curl ... | sh inside it." ;;
    FreeBSD|OpenBSD|NetBSD|DragonFly)
        die "there is no release for ${OS}: V8 has no prebuilt build for it. Run
  feldspar in a Linux VM (bhyve on FreeBSD, vmm on OpenBSD)." ;;
    *)
        die "there is no release for ${OS} on ${ARCH}. Supported:
  ${SUPPORTED}" ;;
esac

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

# What is installed now, if anything: `linux`, or on a host installed before
# there was one release, `static` or `debian13-python`. Either way the upgrade
# is the same tarball.
INSTALLED="$(cat "${PREFIX}/VARIANT" 2>/dev/null || true)"
# Its name carries the platform, ${ARCH} as checked above.
OBJECT="feldspar-linux-${ARCH}.tar.gz"

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

# What the release will run on here — the tarball's install.sh decides,
# by the same tests and by starting the binary; this is the forecast, for the
# log and for --dry-run.
libc_forecast() {
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
    log "upgrading feldspar from ${BASE_URL} (${ID:-unknown} ${ARCH})"
    [ "${INSTALLED}" = linux ] ||
        note "this host has the older ${INSTALLED} release; it moves to the one release, which has Python"
else
    log "installing feldspar from ${BASE_URL} (${ID:-unknown} ${ARCH})"
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
# cannot drift apart. It also links feldspar onto PATH. Its closing advice
# is for a hand install; ours follows below.
if [ "${RESTART}" -eq 1 ]; then
    run_root "${TREE}/install.sh" --no-next-steps
else
    run_root "${TREE}/install.sh" --no-next-steps --no-restart
fi

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
    echo "Nothing else needs to run again: \`feldspar setup\` is for a new host."
    exit 0
fi
cat <<EOF

Next, make this machine an instance — either of:

  sudo feldspar setup --server  a production server: PostgreSQL on this host, a
                                feldspar system account and a service started at boot
  feldspar setup --dev          a development instance: SQLite, and you start
                                \`feldspar serve\` yourself
  feldspar setup                asks which, and which database

\`feldspar setup --help\` lists the options (a PostgreSQL elsewhere, a domain).
To upgrade later, run this installer again: it stops and starts running feldspar
units itself, and keeps the previous release as ${PREFIX}.old.
EOF
