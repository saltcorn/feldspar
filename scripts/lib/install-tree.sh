#!/bin/sh
#
# The release tarball's own installer: `sudo ./install.sh` in an unpacked
# tree. `scripts/build-static.sh` copies this file into every artifact as
# `install.sh`, with @PREFIX@ and @BUILD_GLIBC@ filled in. An operator runs it
# after downloading the tarball by hand, and the curl installer
# (`scripts/install.sh`) runs it too, so there is one way a tree gets installed.
#
# It never writes over the installed tree (D11). A running server holds
# bin/feldspar open, and with the `linux` artifact libpython and (on a musl host)
# libc mapped; writing over those fails ("Text file busy") or crashes the
# process. So:
#
#   1. The tree is copied to ${PREFIX}.new, beside ${PREFIX} and so on the same
#      filesystem. A ${PREFIX}.new an interrupted run left is removed first.
#   2. The `linux` artifact's libc is chosen there (R2.1): the host's glibc
#      when the normal binary starts on it, else the bundled one, by moving the
#      `.glibc` copies over the normal executables. A musl host (Alpine) always
#      gets the bundled one, even with gcompat installed (D6). On a glibc host
#      the `.glibc` copies and glibc/ stay: about 5 MB, and the tree is then
#      the tarball, so a host that later moves to Alpine needs only a reinstall.
#   3. The new binary is started once with no arguments. One that runs on the
#      bundled glibc names its loader at ${PREFIX}/glibc, so it is checked
#      after the swap instead, and a failure there swaps back.
#   4. ${PREFIX}.new/MANIFEST lists every path this install put there.
#   5. Anything under ${PREFIX} that no install put there is copied into the
#      new tree, with a warning naming it: an operator's file must not block an
#      upgrade, and they should hear that it is there. A prefix from before
#      MANIFEST existed is read against the old tarball layout. Caches and
#      build output (__pycache__, *.pyc, and node_modules, build/ or *.egg-info
#      under plugins/) are dropped, not carried.
#   6. Running feldspar.service and feldspar@*.service units are stopped
#      (unless --no-restart), ${PREFIX} is renamed to ${PREFIX}.old and
#      ${PREFIX}.new to ${PREFIX}, and the units are started again. The
#      downtime is two renames.
#   7. ${PREFIX}/bin/feldspar is linked onto PATH as /usr/local/bin/feldspar,
#      so `feldspar setup` is the next command whichever way the tree came.
#
# A process left running (--no-restart) keeps the old inodes until it restarts.
# An interrupted install leaves the old tree whole, and the previous one stays
# as ${PREFIX}.old, so going back is one command, printed at the end.
#
# The prefix is not a choice at install time: the binary carries ${PREFIX}/ui/...
# as the location of its bundles, compiled in, and the `.glibc` copies name their
# loader under it. To install elsewhere, rebuild with
# `scripts/build-static.sh --prefix <path>`.
#
# POSIX sh: Alpine's /bin/sh is busybox ash.
set -eu

PREFIX="@PREFIX@"
# The glibc the normal executables were linked against: the oldest host glibc
# they start on (TODO-portable-release D2).
BUILD_GLIBC="@BUILD_GLIBC@"
# Where the binary is linked onto PATH. Settable only so the tests can keep it
# inside their own directory.
LINK="${FELDSPAR_LINK:-/usr/local/bin/feldspar}"

usage() {
    cat <<EOF
Install this release tree at ${PREFIX}.

Usage: sudo $0 [options]

      --no-restart      Do not stop running feldspar units before the swap or
                        start them after it. They keep running the old release
                        until they are restarted.
      --libc host|bundled
                        Which glibc the linux artifact runs on, instead of
                        deciding: the host's, or the one in glibc/.
      --no-next-steps   Do not print what to run next on a new host (for a
                        caller that prints its own).
  -h, --help            This message.
EOF
}

RESTART=1
LIBC_ARG=""
NEXT_STEPS=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-restart)    RESTART=0; shift ;;
        --libc)
            [ $# -ge 2 ] || { echo "install: --libc needs a value" >&2; exit 2; }
            case "$2" in host|bundled) LIBC_ARG="$2" ;; *)
                echo "install: --libc must be host or bundled, got $2" >&2; exit 2 ;;
            esac
            shift 2 ;;
        --no-next-steps) NEXT_STEPS=0; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               echo "install: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done

log()  { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'install: %s\n' "$*" >&2; exit 1; }

SRC="$(cd -- "$(dirname -- "$0")" && pwd -P)"
NEW="${PREFIX}.new"
OLD="${PREFIX}.old"

[ -f "${SRC}/bin/feldspar" ] || die "${SRC} is not a release tree: it has no bin/feldspar"
PREFIX_REAL="${PREFIX}"
[ -d "${PREFIX}" ] && PREFIX_REAL="$(cd -- "${PREFIX}" && pwd -P)"
case "${SRC}/" in
    "${PREFIX}/"*|"${PREFIX_REAL}/"*|"${NEW}/"*|"${OLD}/"*)
        die "this is an installed copy (${SRC}); run the install.sh of an unpacked release tarball" ;;
esac

if [ "$(id -u)" -ne 0 ] && [ ! -w "$(dirname -- "${PREFIX}")" ]; then
    die "${PREFIX} needs root (re-run with sudo)"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/feldspar-install-tree.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT INT TERM

exists() { [ -e "$1" ] || [ -L "$1" ]; }

# `feldspar --version`'s line, or what to say instead: a build from before the
# flag answers it with "unknown command".
version_of() {
    "$1" --version 2>/dev/null | head -n 1 | grep . || echo "an earlier build (no --version)"
}

UPGRADE=0
if [ -x "${PREFIX}/bin/feldspar" ]; then
    UPGRADE=1
    log "upgrading ${PREFIX}: $(version_of "${PREFIX}/bin/feldspar")"
    note "docs/OPERATIONS.md §3.1 asks for a database dump before every upgrade"
fi

# ---------------------------------------------------------------------------
# 1. The new tree, beside the old one
# ---------------------------------------------------------------------------

if exists "${NEW}"; then
    note "removing ${NEW}, left by an interrupted install"
    rm -rf "${NEW}"
fi
mkdir -p "$(dirname -- "${PREFIX}")"
mkdir "${NEW}"
# -p keeps the timestamps, which is what keeps the bundled CPython's .pyc files
# valid: a copy with new mtimes makes every one of them stale, and Python then
# recompiles the standard library at each start (or writes into the prefix,
# when root runs it).
cp -pR "${SRC}/." "${NEW}/"
# Root's, and nobody else's to write: the tree was unpacked by whoever ran the
# downloader, and the service account must not be able to rewrite the code it
# executes.
if [ "$(id -u)" -eq 0 ]; then
    chown -hR 0:0 "${NEW}"
fi
chmod -R go-w "${NEW}"

# ---------------------------------------------------------------------------
# 2. Which libc (the linux artifact)
# ---------------------------------------------------------------------------

# `musl`, `glibc <version>`, or nothing when it cannot tell. musl first, and by
# what the host's own tools say (D6): Alpine with gcompat has a glibc loader
# path, and a Debian host with the `musl` package has /lib/ld-musl-*.
host_libc() {
    if ldd --version 2>&1 | grep -qi musl; then
        echo musl
        return 0
    fi
    _v="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
    if [ -n "${_v}" ]; then
        echo "glibc ${_v#glibc }"
        return 0
    fi
    if ls /lib/ld-musl-* >/dev/null 2>&1; then
        echo musl
    fi
    return 0
}

# Start a binary with no arguments (it prints its usage and exits 0); on
# failure, its first line of complaint is in ${WORK}/probe.
starts() {
    "$1" >/dev/null 2>"${WORK}/probe"
}
complaint() {
    head -n 1 "${WORK}/probe" 2>/dev/null || true
}

LIBC=""
LIBC_REASON=""
CHECKED=0
if [ -d "${NEW}/glibc" ] && [ -f "${NEW}/bin/feldspar.glibc" ]; then
    if [ -n "${LIBC_ARG}" ]; then
        LIBC="${LIBC_ARG}"
        LIBC_REASON="chosen with --libc"
        if [ "${LIBC}" = host ]; then
            starts "${NEW}/bin/feldspar" ||
                die "--libc host, but the new binary does not start here: $(complaint)"
            CHECKED=1
        fi
    else
        found="$(host_libc)"
        if [ "${found}" = musl ]; then
            LIBC="bundled"
            LIBC_REASON="this is a musl host"
        elif starts "${NEW}/bin/feldspar"; then
            LIBC="host"
            LIBC_REASON="the host's ${found:-glibc}"
            CHECKED=1
        else
            LIBC="bundled"
            LIBC_REASON="the normal binary does not start on the host's ${found:-libc} (it needs glibc ${BUILD_GLIBC} or newer): $(complaint)"
        fi
    fi
    if [ "${LIBC}" = bundled ]; then
        # The copies replace the files the paths name, so a systemd unit, the
        # hosted setup's ExecStart and --python-bin need nothing changed.
        # `python3` is a link to `python3.13`: the file is what is replaced.
        mv -f "${NEW}/bin/feldspar.glibc" "${NEW}/bin/feldspar"
        python="$(readlink -f "${NEW}/python/bin/python3")"
        mv -f "${NEW}/python/bin/python3.glibc" "${python}"
        log "this install runs on the bundled glibc in ${PREFIX}/glibc: ${LIBC_REASON}"
    else
        log "this install runs on ${LIBC_REASON}"
    fi
    printf '%s: %s\n' "${LIBC}" "${LIBC_REASON}" > "${NEW}/LIBC"
fi

# ---------------------------------------------------------------------------
# 3. The new binary starts (the bundled-glibc one: after the swap)
# ---------------------------------------------------------------------------

if [ "${LIBC}" != bundled ] && [ "${CHECKED}" -eq 0 ]; then
    starts "${NEW}/bin/feldspar" || die "the new binary does not start here: $(complaint)
  ${PREFIX} is unchanged; the new tree was left at ${NEW}"
fi

# ---------------------------------------------------------------------------
# 4. What this install put there
# ---------------------------------------------------------------------------

(cd "${NEW}" && { echo MANIFEST; find . | sed -n 's,^\./,,p'; } | LC_ALL=C sort -u) \
    > "${WORK}/MANIFEST"
mv "${WORK}/MANIFEST" "${NEW}/MANIFEST"

# ---------------------------------------------------------------------------
# 5. What an operator put there
# ---------------------------------------------------------------------------

CARRIED=0
if [ -d "${PREFIX}" ]; then
    legacy=1
    : > "${WORK}/owned"
    if [ -f "${PREFIX}/MANIFEST" ]; then
        legacy=0
        cp "${PREFIX}/MANIFEST" "${WORK}/owned"
    fi
    (cd "${PREFIX}" && find . | sed -n 's,^\./,,p') > "${WORK}/present"
    # The topmost paths no install put there, in `find`'s order (a directory
    # before what is in it), so a foreign directory is named once rather than
    # file by file. Without a MANIFEST, what an install put there is the layout
    # every earlier tarball had, including R1's interrupted python/glibc swaps.
    awk -v legacy="${legacy}" -v ownedfile="${WORK}/owned" '
        BEGIN {
            while ((getline line < ownedfile) > 0) owned[line] = 1
            n = split("bin ui plugins python glibc python.new python.old glibc.new glibc.old VARIANT setup-host.sh LIBC", l, " ")
            for (i = 1; i <= n; i++) layout[l[i]] = 1
        }
        {
            p = $0
            parent = p
            if (!sub(/\/[^\/]*$/, "", parent)) parent = ""
            if (parent in covered) { covered[p] = 1; next }
            if (legacy) {
                top = p
                sub(/\/.*/, "", top)
                if (top in layout) next
            } else if (p in owned) next
            covered[p] = 1
            if (p ~ /(^|\/)__pycache__$/ || p ~ /\.pyc$/ ||
                p ~ /^plugins\/[^\/]+\/(node_modules|build|[^\/]*\.egg-info)$/)
                print "drop " p
            else
                print "keep " p
        }' "${WORK}/present" > "${WORK}/foreign"

    while IFS= read -r line; do
        p="${line#* }"
        if [ "${line%% *}" = drop ]; then
            note "not carrying over ${PREFIX}/${p}: a cache or build output"
            continue
        fi
        if exists "${NEW}/${p}"; then
            warn "${PREFIX}/${p} was not put there by an install, and the new release ships its own; the new one is installed (the old one stays in ${OLD})"
            continue
        fi
        mkdir -p "$(dirname -- "${NEW}/${p}")"
        cp -pR "${PREFIX}/${p}" "${NEW}/${p}"
        warn "carrying over ${PREFIX}/${p}: no install put it there"
        CARRIED=$((CARRIED + 1))
    done < "${WORK}/foreign"
fi

# ---------------------------------------------------------------------------
# 6. Stop, swap, start
# ---------------------------------------------------------------------------

STOPPED=""
if [ "${RESTART}" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
    running="$(systemctl list-units --type=service --state=active --no-legend --plain \
        'feldspar.service' 'feldspar@*.service' 2>/dev/null | awk '{ print $1 }')" || running=""
    for unit in ${running}; do
        log "stopping ${unit}"
        if systemctl stop "${unit}"; then
            STOPPED="${STOPPED} ${unit}"
        else
            warn "could not stop ${unit}; installing anyway, and not starting it again"
        fi
    done
fi

start_stopped() {
    for unit in ${STOPPED}; do
        log "starting ${unit}"
        systemctl start "${unit}" || warn "${unit} did not start: journalctl -u ${unit}"
    done
}

# Back to the old tree after the new one went in, with the new one at ${NEW}.
swap_back() {
    mv "${PREFIX}" "${NEW}"
    if [ "${HAD_OLD}" -eq 1 ]; then
        mv "${OLD}" "${PREFIX}"
    fi
}

if exists "${OLD}"; then
    rm -rf "${OLD}"
fi
HAD_OLD=0
if exists "${PREFIX}"; then
    if ! mv "${PREFIX}" "${OLD}"; then
        start_stopped
        die "could not rename ${PREFIX} (is it a mount point?); it is unchanged, and the new tree is at ${NEW}"
    fi
    HAD_OLD=1
fi
if ! mv "${NEW}" "${PREFIX}"; then
    [ "${HAD_OLD}" -eq 1 ] && mv "${OLD}" "${PREFIX}"
    start_stopped
    die "could not rename ${NEW} to ${PREFIX}; ${PREFIX} is as it was"
fi

if [ "${LIBC}" = bundled ] && ! starts "${PREFIX}/bin/feldspar"; then
    swap_back
    start_stopped
    die "the new binary does not start on its bundled glibc: $(complaint)
  ${PREFIX} is as it was; the new tree was left at ${NEW}"
fi

start_stopped

mkdir -p "$(dirname "${LINK}")"
ln -sfn "${PREFIX}/bin/feldspar" "${LINK}"

# ---------------------------------------------------------------------------
# What happened
# ---------------------------------------------------------------------------

log "installed ${PREFIX}: $(version_of "${PREFIX}/bin/feldspar"), on PATH as ${LINK}"
[ "${CARRIED}" -eq 0 ] || note "${CARRIED} path(s) carried over from the previous tree; see the warnings above"
if [ "${HAD_OLD}" -eq 1 ]; then
    if [ -n "${STOPPED}" ]; then
        note "restarted:${STOPPED}"
    elif [ "${RESTART}" -eq 0 ]; then
        note "--no-restart: running feldspar processes keep the old release until they restart"
    fi
    note "the previous tree is ${OLD}; to go back to it:"
    note "  mv ${PREFIX} ${PREFIX}.bad && mv ${OLD} ${PREFIX}"
fi

if [ "${NEXT_STEPS}" -eq 1 ] && [ "${UPGRADE}" -eq 0 ]; then
    cat <<EOF

next, make this machine an instance — either of:
  sudo feldspar setup --server --domain example.com
                                a production server: PostgreSQL, Node.js, a
                                headless browser, a feldspar system account,
                                /etc/feldspar/feldspar.toml and a service
  feldspar setup --dev          a development instance: SQLite, and you start
                                \`feldspar serve\` yourself
\`feldspar setup --help\` lists the options; --dry-run prints what it would do.

To upgrade later, run the install.sh of a newer tarball the same way: it stops
and starts running feldspar units itself.
EOF
fi
