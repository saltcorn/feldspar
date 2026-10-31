#!/bin/sh
#
# The Python half of the `linux` release artifact (TODO-portable-release R1.1,
# R1.2), shared by `scripts/static-build.Dockerfile` and a `--native` build of
# `scripts/build-static.sh` so the two cannot assemble different trees.
#
#   python-bundle.sh fetch URL SHA256 DEST RECORDED
#
#       Download python-build-standalone's `install_only_stripped` tarball,
#       refuse it unless its SHA-256 is the pinned one, and unpack it as
#       DEST/python. Then trim it — Tcl/Tk and IDLE, which a server never runs
#       — and rewrite the `/install` placeholder PBS's build left in
#       `sysconfig` (and `python3-config`, the Makefile and the pkg-config
#       files) to RECORDED, the directory it will be installed at. pip reads
#       those values when it builds a C extension from source, and `-I/install`
#       finds no headers.
#
#   python-bundle.sh glibc TREE PREFIX STRIP
#
#       TREE holds bin/feldspar and python/. Strip the binary when STRIP is 1,
#       then copy every library the binary, the interpreter and libpython load
#       from outside TREE — the build machine's glibc, its loader and
#       libgcc_s, found with `ldd` rather than listed — plus glibc's own NSS
#       modules (compat, db) to TREE/glibc. Make bin/feldspar.glibc and
#       python/bin/python3.glibc: the same files, with the loader set to the
#       bundled one and a DT_RPATH — not RUNPATH, which does not reach NSS
#       modules and other dlopens — of PREFIX/glibc:PREFIX/python/lib. Those
#       copies are what a musl host (Alpine) runs; the paths are absolute, which
#       is why the prefix is compiled in (TODO-portable-release §2, D3).
#
# POSIX sh, GNU userland: it runs on the Debian build image and on a developer's
# Linux machine. Needs curl, sha256sum and tar for `fetch`; ldd, patchelf and
# (with STRIP=1) strip for `glibc`.
set -eu

die() { echo "python-bundle: $*" >&2; exit 1; }

cmd_fetch() {
    [ $# -eq 4 ] || die "usage: fetch URL SHA256 DEST RECORDED"
    url="$1" sha="$2" dest="$3" recorded="$4"
    [ -n "${sha}" ] || die "no SHA-256 pinned for ${url}"
    work="$(mktemp -d "${TMPDIR:-/tmp}/python-bundle.XXXXXX")"
    trap 'rm -rf "${work}"' EXIT
    echo "python-bundle: fetching ${url}"
    curl -fsSL --retry 3 -o "${work}/pbs.tar.gz" "${url}"
    got="$(sha256sum "${work}/pbs.tar.gz" | cut -d' ' -f1)"
    [ "${got}" = "${sha}" ] ||
        die "${url} has SHA-256 ${got}, not the pinned ${sha}; refusing it"
    rm -rf "${dest}/python"
    mkdir -p "${dest}"
    tar -xzf "${work}/pbs.tar.gz" -C "${dest}"
    py="${dest}/python"
    [ -d "${py}/lib" ] || die "${url} did not unpack to python/"

    # Tcl/Tk (tkinter, IDLE, turtledemo) needs a display and is ~10 MB; the
    # manual pages are for a terminal nobody opens here. The headers stay: pip
    # needs them to build an extension from an sdist.
    rm -rf "${py}"/lib/libtcl* "${py}"/lib/libtk* "${py}"/lib/tcl* "${py}"/lib/tk* \
        "${py}"/lib/itcl* "${py}"/lib/thread* \
        "${py}"/lib/python3.*/tkinter "${py}"/lib/python3.*/idlelib \
        "${py}"/lib/python3.*/turtledemo "${py}"/lib/python3.*/lib-dynload/_tkinter.* \
        "${py}"/bin/idle* "${py}"/share/man

    # `/install` as a whole path component, at the start of a value or after a
    # quote, `=`, blank or `-I`/`-L`-style flag letter — never the tail of
    # `/usr/bin/install`. Twice, because a match consumes the blank the next
    # one would start at.
    for f in "${py}"/lib/python3.*/_sysconfigdata_*.py "${py}"/bin/python3*-config \
             "${py}"/lib/python3.*/config-3.*/Makefile "${py}"/lib/pkgconfig/*.pc; do
        if [ ! -f "${f}" ] || [ -L "${f}" ]; then continue; fi
        for _ in 1 2; do
            sed -i \
                -e "s#\(^\|[=\"'[:blank:]]\|-[ILR]\)/install\([/\"'[:blank:]]\|\$\)#\1${recorded}\2#g" \
                "${f}"
        done
    done
    # The whole library's bytecode is compiled here, by the interpreter that
    # will read it, and shipped. `install_only_stripped` has none, and without
    # it the first root process to import a module writes `__pycache__` into
    # the installed prefix, which an install treats as read-only
    # (TODO-portable-release R2.6), while the service account, which cannot
    # write there, compiles the stdlib again on every start. The rewritten
    # sysconfig's bytecode is stale, so it goes first. The .pyc files are
    # checked by timestamp, which tar and install.sh (`cp -p`) both keep.
    find "${py}/lib" -path '*/__pycache__/_sysconfigdata*' -delete 2>/dev/null || true
    "${py}/bin/python3" -I -m compileall -q -j 0 "${py}"/lib/python3.* > /dev/null ||
        die "could not compile the bundled library's bytecode"

    ls "${py}"/lib/libpython3.*.so.1.0 >/dev/null 2>&1 ||
        die "${url} has no shared libpython under python/lib"
    echo "python-bundle: $(basename "${url}") unpacked at ${py}, recorded as ${recorded}"
}

# `ldd FILE`, as "SONAME PATH" lines for every library it loads (the loader as
# "LOADER PATH"), skipping the vDSO. A library ldd cannot find is an error: the
# bundle would be missing it.
closure() {
    ldd "$1" | while read -r first second third _; do
        case "${first}" in
            linux-vdso.so.*|linux-gate.so.*) ;;
            /*) echo "LOADER ${first}" ;;
            *)
                [ "${second}" = "=>" ] || continue
                [ "${third}" != "not" ] || die "$1 needs ${first}, which ldd cannot find"
                echo "${first} ${third}"
                ;;
        esac
    done
}

cmd_glibc() {
    [ $# -eq 3 ] || die "usage: glibc TREE PREFIX STRIP"
    tree="$1" prefix="$2" strip="$3"
    bin="${tree}/bin/feldspar"
    [ -f "${bin}" ] || die "${bin} is missing"
    [ -e "${tree}/python/bin/python3" ] || die "${tree}/python/bin/python3 is missing"
    python="$(readlink -f "${tree}/python/bin/python3")"
    tree_real="$(cd "${tree}" && pwd -P)"

    if [ "${strip}" = 1 ]; then
        strip "${bin}"
    fi

    glibc="${tree}/glibc"
    rm -rf "${glibc}"
    mkdir -p "${glibc}"
    loader=""
    libc_dir=""
    for elf in "${bin}" "${python}" "${tree}"/python/lib/libpython3.*.so.1.0 \
               "${tree}"/python/lib/python3.*/lib-dynload/*.so; do
        [ -f "${elf}" ] || continue
        closure "${elf}" > "${glibc}/.closure" || exit 1
        while read -r name path; do
            real="$(readlink -f "${path}")"
            case "${real}" in "${tree_real}"/*) continue ;; esac
            if [ "${name}" = LOADER ]; then
                name="$(basename "${path}")"
                if [ -n "${loader}" ] && [ "${loader}" != "${name}" ]; then
                    die "two loaders in one bundle: ${loader} and ${name}"
                fi
                loader="${name}"
            fi
            [ "${name}" = libc.so.6 ] && libc_dir="$(dirname "${real}")"
            [ -e "${glibc}/${name}" ] || cp -L "${path}" "${glibc}/${name}"
        done < "${glibc}/.closure"
    done
    rm -f "${glibc}/.closure"
    [ -n "${loader}" ] || die "ldd named no loader for ${bin}"
    [ -n "${libc_dir}" ] || die "ldd named no libc.so.6 for ${bin}"

    # glibc's own NSS modules: `files` and `dns` are inside libc since 2.34,
    # and these two are what a passwd/group line commonly adds (openSUSE's
    # `compat`). Under DT_RPATH they are found here before anywhere on the host.
    for module in libnss_compat.so.2 libnss_db.so.2; do
        if [ -e "${libc_dir}/${module}" ]; then
            cp -L "${libc_dir}/${module}" "${glibc}/${module}"
        else
            echo "python-bundle: warning: ${libc_dir}/${module} is not on this machine; not bundled" >&2
        fi
    done

    rpath="${prefix}/glibc:${prefix}/python/lib"
    cp "${bin}" "${bin}.glibc"
    cp "${python}" "${tree}/python/bin/python3.glibc"
    for copy in "${bin}.glibc" "${tree}/python/bin/python3.glibc"; do
        patchelf --set-interpreter "${prefix}/glibc/${loader}" \
            --force-rpath --set-rpath "${rpath}" "${copy}"
        chmod 755 "${copy}"
    done
    echo "python-bundle: bundled $(ls "${glibc}" | wc -l) libraries in glibc/; loader ${prefix}/glibc/${loader}"
}

case "${1:-}" in
    fetch) shift; cmd_fetch "$@" ;;
    glibc) shift; cmd_glibc "$@" ;;
    *) die "usage: python-bundle.sh fetch URL SHA256 DEST RECORDED | glibc TREE PREFIX STRIP" ;;
esac
