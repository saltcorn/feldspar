#!/bin/sh
#
# The `linux` artifact's smoke test, run as root inside one distribution image
# by `scripts/build-static.sh` (TODO-portable-release R2.4, R2.6):
#
#   smoke-linux.sh RELEASE_DIR OBJECT PREFIX [--venv] [--upgrade]
#
# RELEASE_DIR holds OBJECT and OBJECT.sha256, as the bucket does, and
# /install.sh is the curl installer (scripts/install.sh). The install is that
# installer with `--base-url file://RELEASE_DIR`, so what is tested is what an
# operator runs: the family detection and package step (R2.1), the checksum,
# and the tarball's own install.sh with its libc choice and tree swap.
#
# Then, on every image:
#   - `feldspar python check` passes, the HTTPS request included. Where that
#     needs SSL_CERT_FILE to pass, it is reported (`NEEDS SSL_CERT_FILE`) and
#     not failed: setup-host.sh writes it into the unit on such a host (D10).
#   - the process maps the host's libc.so.6 on a glibc image, and the bundle's
#     on a musl one (the self-check's `libc` line is read off /proc/self/maps).
# With --venv: the bundled `python3 -m venv` and `pip install` of a pure-Python
# package (six) and a manylinux wheel (markupsafe, with its C speedups), and the
# embedded interpreter imports both.
# With --upgrade: an older tree is installed first, with a server running from
# it, a file only it has, and a file an operator added; the install over it must
# succeed, leave the server running and the old tree in PREFIX.old, drop the
# old-only file, keep the operator's with a warning — and a second install of
# the same tarball must change nothing but PREFIX.old.
#
# POSIX sh: it runs on Alpine's busybox too.
set -eu

[ $# -ge 3 ] || { echo "usage: smoke-linux.sh RELEASE_DIR OBJECT PREFIX [--venv] [--upgrade]" >&2; exit 2; }
release="$1" object="$2" prefix="$3"
shift 3
venv=0
upgrade=0
for arg; do
    case "${arg}" in
        --venv)    venv=1 ;;
        --upgrade) upgrade=1 ;;
        *) echo "smoke: unknown option ${arg}" >&2; exit 2 ;;
    esac
done

fail() { printf 'smoke FAILED: %s\n' "$*" >&2; exit 1; }
say()  { printf 'smoke: %s\n' "$*"; }

feldspar="${prefix}/bin/feldspar"
work="$(mktemp -d)"

# The tree as a reproducible listing: every path with its type and, for a file,
# its checksum — what "changes nothing" is measured against.
snapshot() {
    (cd "$1" && find . | LC_ALL=C sort | while IFS= read -r p; do
        if [ -L "${p}" ]; then
            echo "${p} -> $(readlink "${p}")"
        elif [ -f "${p}" ]; then
            echo "${p} $(cksum < "${p}")"
        else
            echo "${p}/"
        fi
    done)
}

# ---------------------------------------------------------------------------
# --upgrade: an older release first, running
# ---------------------------------------------------------------------------

server=""
if [ "${upgrade}" -eq 1 ]; then
    mkdir -p "${work}/old"
    tar -xzf "${release}/${object}" -C "${work}/old"
    old_tree="$(echo "${work}"/old/feldspar-*)"
    echo "only the older release has this" > "${old_tree}/old-only.txt"
    say "installing an older tree (with old-only.txt)"
    "${old_tree}/install.sh" --no-next-steps > "${work}/old-install.log" 2>&1 ||
        { cat "${work}/old-install.log" >&2; fail "the first install failed"; }
    "${feldspar}" serve --sqlite "${work}/smoke.db" --bind 127.0.0.1:18080 \
        > "${work}/serve.log" 2>&1 &
    server=$!
    sleep 5
    kill -0 "${server}" 2>/dev/null ||
        { cat "${work}/serve.log" >&2; fail "the server from the older tree did not stay up"; }
    echo "an operator's file" > "${prefix}/local.conf"
fi

# ---------------------------------------------------------------------------
# The install, as an operator runs it
# ---------------------------------------------------------------------------

say "installing with the curl installer from file://${release}"
if [ "${prefix}" = /opt/feldspar ]; then
    sh /install.sh --base-url "file://${release}" --variant linux > "${work}/install.log" 2>&1 ||
        { cat "${work}/install.log" >&2; fail "the curl installer failed"; }
else
    # The curl installer knows only /opt/feldspar; another prefix gets the
    # tarball's own installer, and no packages.
    mkdir -p "${work}/new"
    tar -xzf "${release}/${object}" -C "${work}/new"
    "$(echo "${work}"/new/feldspar-*)/install.sh" > "${work}/install.log" 2>&1 ||
        { cat "${work}/install.log" >&2; fail "install.sh failed"; }
fi
cat "${work}/install.log"
[ -f "${prefix}/LIBC" ] || fail "the install wrote no ${prefix}/LIBC"
say "libc choice: $(cat "${prefix}/LIBC")"

# ---------------------------------------------------------------------------
# The self-check, and which libc the process maps
# ---------------------------------------------------------------------------

musl=0
if ldd --version 2>&1 | grep -qi musl; then musl=1; fi

check_ok=0
if "${feldspar}" python check > "${work}/check.log" 2>&1; then
    check_ok=1
else
    for bundle in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
                  /etc/ssl/ca-bundle.pem /etc/ssl/cert.pem; do
        [ -f "${bundle}" ] || continue
        if SSL_CERT_FILE="${bundle}" "${feldspar}" python check > "${work}/check-cert.log" 2>&1; then
            say "NEEDS SSL_CERT_FILE=${bundle}: the HTTPS check passes only with it"
            cp "${work}/check-cert.log" "${work}/check.log"
            check_ok=1
            break
        fi
    done
fi
cat "${work}/check.log"
[ "${check_ok}" -eq 1 ] || fail "feldspar python check failed"

libc="$(sed -n 's/^  libc  *//p' "${work}/check.log")"
[ -n "${libc}" ] || fail "the self-check named no mapped libc"
case "${musl}:${libc}" in
    1:"${prefix}/glibc/"*) say "maps the bundled ${libc}, as a musl host must" ;;
    1:*)                   fail "a musl host should map ${prefix}/glibc/libc.so.6, not ${libc}" ;;
    0:"${prefix}/"*)       fail "a glibc host should map its own libc, not the bundle's ${libc}" ;;
    0:*)                   say "maps the host's ${libc}" ;;
esac

# ---------------------------------------------------------------------------
# --venv: the bundled interpreter's venv and pip, and a wheel imported here
# ---------------------------------------------------------------------------

if [ "${venv}" -eq 1 ]; then
    say "venv + pip install six markupsafe"
    "${prefix}/python/bin/python3" -m venv "${work}/py" ||
        fail "the bundled python3 could not make a venv"
    "${work}/py/bin/pip" install --disable-pip-version-check -q six markupsafe ||
        fail "pip install six markupsafe failed"
    "${feldspar}" python check --offline --python-dir "${work}/py" \
        --import six --import markupsafe._speedups > "${work}/import.log" 2>&1 ||
        { cat "${work}/import.log" >&2; fail "the embedded interpreter could not import what pip installed"; }
    cat "${work}/import.log"
fi

# ---------------------------------------------------------------------------
# --upgrade: what the swap must and must not have done
# ---------------------------------------------------------------------------

if [ "${upgrade}" -eq 1 ]; then
    kill -0 "${server}" 2>/dev/null ||
        { cat "${work}/serve.log" >&2; fail "the server running from the old tree died during the install"; }
    maps="$(awk '$6 ~ /\/libc\.so\.6$/ { print $6; exit }' "/proc/${server}/maps")"
    case "${musl}:${maps}" in
        1:"${prefix}/glibc/libc.so.6"|0:/*) say "the running server maps ${maps}" ;;
        *) fail "the running server maps ${maps:-no libc.so.6}" ;;
    esac
    [ -f "${prefix}.old/old-only.txt" ] || fail "${prefix}.old does not hold the old tree"
    [ ! -e "${prefix}/old-only.txt" ] || fail "a file only the old release had is still in ${prefix}"
    [ "$(cat "${prefix}/local.conf" 2>/dev/null)" = "an operator's file" ] ||
        fail "the operator's local.conf was not carried over"
    grep -q "carrying over ${prefix}/local.conf" "${work}/install.log" ||
        fail "the install did not warn about local.conf"
    say "upgrade: server still running, old tree in ${prefix}.old, old-only.txt gone, local.conf kept"

    snapshot "${prefix}" > "${work}/before"
    say "installing the same tarball again"
    sh /install.sh --base-url "file://${release}" --variant linux > "${work}/again.log" 2>&1 ||
        { cat "${work}/again.log" >&2; fail "the second install failed"; }
    snapshot "${prefix}" > "${work}/after"
    diff "${work}/before" "${work}/after" ||
        fail "a second install of the same tarball changed ${prefix}"
    snapshot "${prefix}.old" > "${work}/old-now"
    diff "${work}/before" "${work}/old-now" >/dev/null ||
        fail "after the second install, ${prefix}.old should be the tree it replaced"
    say "a second install changed nothing but ${prefix}.old"
    kill "${server}" 2>/dev/null || true
fi

say "passed"
