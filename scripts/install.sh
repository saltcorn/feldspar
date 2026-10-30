#!/bin/sh
#
# Install the `feldspar` executable — and only that — from the latest static
# release:
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
# What it installs beside the binary is what the binary needs to *run* and what
# this script needs to fetch it: ca-certificates (the server makes outbound TLS
# connections — ACME, LLM providers, SMTP), a downloader and tar. The binary is
# statically linked, so there is no library to add. PostgreSQL, Node.js and a
# browser are `feldspar setup`'s business, or scripts/setup-host.sh's.
#
# Debian, Ubuntu and Alpine on x86_64, which is what the release is built and
# smoke-tested for (scripts/build-static.sh). Anything else is refused by name
# rather than half-installed; `--force` tries anyway, for a derivative that the
# /etc/os-release check does not recognise.
#
# Plain POSIX sh, not bash: Alpine's /bin/sh is busybox ash, and bash is one of
# the things it does not have. Everything after `sh -s --` is an option of this
# script; `--help` lists them, and `--dry-run` prints the commands without
# running any.
#
set -eu

BASE_URL="${FELDSPAR_BASE_URL:-https://feldspar-latest-static.saltcorn.com}"
OBJECT="feldspar.tar.gz"
# Fixed by the build: the binary carries ${PREFIX}/ui/... as where its bundles
# are, so the tree cannot go anywhere else (build-static.sh --prefix).
PREFIX="/opt/feldspar"
LINK="/usr/local/bin/feldspar"
DRY_RUN=0
FORCE=0
SKIP_PACKAGES=0

usage() {
    cat <<EOF
Install the feldspar executable from the latest release.

Usage: curl -fsSL ${BASE_URL}/install.sh | sh -s -- [options]

      --base-url URL    Where to download ${OBJECT} from (default ${BASE_URL},
                        or \$FELDSPAR_BASE_URL).
      --no-packages     Do not install ca-certificates, curl/wget or tar with the
                        package manager; fail instead if a needed tool is missing.
      --force           Install on a distribution or architecture this script
                        does not recognise.
  -n, --dry-run         Print what would be done, do nothing.
  -h, --help            This message.

Then: feldspar setup --dev, or sudo feldspar setup --server.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --base-url)
            [ $# -ge 2 ] || { echo "error: --base-url needs a value" >&2; exit 2; }
            BASE_URL="$2"; shift 2 ;;
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
# Is this a machine the release runs on?
# ---------------------------------------------------------------------------

ID=""
ID_LIKE=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
fi
FAMILY=""
case "${ID}" in
    debian|ubuntu) FAMILY="debian" ;;
    alpine)        FAMILY="alpine" ;;
    *)
        case " ${ID_LIKE} " in
            *" debian "*|*" ubuntu "*) FAMILY="debian" ;;
            *" alpine "*)              FAMILY="alpine" ;;
        esac
        ;;
esac
if [ -z "${FAMILY}" ]; then
    [ "${FORCE}" -eq 1 ] ||
        die "this installer knows Debian, Ubuntu and Alpine, and this is '${ID:-unknown}'.
  The release is a static binary and may well run here: --force installs it
  anyway, without the package manager (so ca-certificates, tar and curl or wget
  must already be here)."
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

log "installing feldspar from ${BASE_URL} (${ID:-unknown} ${ARCH})"
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

PACKAGES=""
[ -e /etc/ssl/certs/ca-certificates.crt ] || PACKAGES="ca-certificates"
command -v tar >/dev/null 2>&1 || PACKAGES="${PACKAGES} tar"
command -v gzip >/dev/null 2>&1 || PACKAGES="${PACKAGES} gzip"
# Alpine's busybox wget speaks TLS; Debian's minimal images have neither tool.
[ -n "${DOWNLOADER}" ] || { PACKAGES="${PACKAGES} curl"; DOWNLOADER="curl"; }
PACKAGES="${PACKAGES# }"

if [ -n "${PACKAGES}" ]; then
    if [ "${SKIP_PACKAGES}" -eq 1 ]; then
        die "missing: ${PACKAGES}; install them, or drop --no-packages"
    fi
    log "installing ${PACKAGES}"
    case "${FAMILY}" in
        debian)
            run_root apt-get update -qq
            run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ${PACKAGES}
            ;;
        alpine)
            run_root apk add --no-cache ${PACKAGES}
            ;;
    esac
fi

fetch() {  # fetch URL FILE
    if [ "${DRY_RUN}" -eq 1 ]; then
        printf '    $ %s %s -> %s\n' "${DOWNLOADER}" "$1" "$2"
        return 0
    fi
    case "${DOWNLOADER}" in
        curl) curl -fsSL --proto-redir '=https' -o "$2" "$1" ;;
        wget) wget -qO "$2" "$1" ;;
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
# The tarball's own installer: it knows the tree's layout (bin/, ui/, plugins/),
# and is what an upgrade runs, so the two install paths cannot drift apart. Its
# closing advice is about setup-host.sh; ours follows below.
if [ "${DRY_RUN}" -eq 1 ]; then
    run_root "${TREE}/install.sh"
else
    run_root "${TREE}/install.sh" >/dev/null
fi
run_root mkdir -p "$(dirname "${LINK}")"
run_root ln -sfn "${PREFIX}/bin/feldspar" "${LINK}"

if [ "${DRY_RUN}" -eq 1 ]; then
    log "dry run finished; nothing above was executed"
    exit 0
fi

log "installed ${PREFIX}/bin/feldspar, on PATH as ${LINK}"
cat <<EOF

Next, make this machine an instance — either of:

  feldspar setup --dev          a development instance: SQLite, and you start
                                \`feldspar serve\` yourself
  sudo feldspar setup --server  a production server: PostgreSQL on this host, a
                                feldspar system account and a service started at boot
  feldspar setup                asks which, and which database

\`feldspar setup --help\` lists the options (a PostgreSQL elsewhere, a domain).
To upgrade later, run this installer again and restart the service.
EOF
