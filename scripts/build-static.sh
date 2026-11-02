#!/usr/bin/env bash
#
# Build the `feldspar` release: the tarball `scripts/install.sh` downloads, and
# that an operator can download and install by hand (docs/OPERATIONS.md §2).
#
# What it is
# ----------
# One artifact per architecture, dynamically linked, built on Debian 12
# (bookworm, glibc 2.36) with `--features python` against a **bundled** CPython
# 3.13 — python-build-standalone, pinned below by release and SHA-256 — that
# ships in the tarball as `python/`. The binary finds it through
# `$ORIGIN/../python/lib`, so the host needs nothing Python-related, and pip runs
# that same interpreter. What comes from the host is glibc ≥ 2.36, which every
# supported distribution but Alpine has. For Alpine (and older glibc) the
# tarball also carries bookworm's glibc in `glibc/` and `.glibc` copies of the
# two executables, whose loader and DT_RPATH point there
# (`scripts/lib/python-bundle.sh`); the tarball's `install.sh` picks one.
#
# Why there is no musl build: `deno_core` links **V8**, and V8 reaches this build
# as `rusty_v8`'s prebuilt static archive, which upstream publishes for
# `*-unknown-linux-gnu`, `*-apple-darwin` and Windows only. Building V8 from
# source against musl is not a supported configuration, so the musl targets are
# refused by name below rather than failing forty minutes into a build.
#
# Names are resolved by `crates/sc-dns`, in Rust, rather than by glibc's NSS, so
# /etc/nsswitch.conf does not apply (README §4.1).
#
# What it produces
# ----------------
#   dist/feldspar-<version>-<target>.tar.gz   and a .sha256 beside it
#
# and, with `--release`, publishes it under names with no version but with the
# platform, each from the build machine of its own architecture:
#
#   feldspar-linux-<arch>.tar.gz   the release
#   feldspar.tar.gz                (x86_64) the same file, under the name the
#                                  first releases had, which existing scripts
#                                  and hosts fetch
#
# unpacking to the **install prefix** (`/opt/feldspar` by default):
#
#   /opt/feldspar/bin/feldspar        the binary
#   /opt/feldspar/python/             the bundled CPython it links
#   /opt/feldspar/glibc/              glibc for bin/*.glibc, for musl hosts
#   /opt/feldspar/ui/admin/dist       the admin SPA it serves
#   /opt/feldspar/ui/ide/dist         the file-store IDE it serves
#   /opt/feldspar/ui/saltcorn-ui/dist Saltcorn UI's view runtime and assets
#   /opt/feldspar/ui/builder/dist     Saltcorn UI's builder, the layout editor
#   /opt/feldspar/ui/analytics/dist   the Analytics UI, served under /analytics/
#   /opt/feldspar/plugins/            the modules it ships with, installable in
#                                     one click from Settings -> Modules
#   /opt/feldspar/install.sh          installs the tree by swapping it in
#                                     (scripts/lib/install-tree.sh)
#   /opt/feldspar/MANIFEST, LIBC      written by it: what it installed, and
#                                     which glibc the install runs on
#
# A new host then runs `sudo feldspar setup --server` (crates/sc-cli/src/setup.rs).
#
# The prefix is not cosmetic: `crates/sc-cli/build.rs` compiles the bundle paths
# into the binary, and the IDE's has no run-time flag to override it. This script
# therefore builds with `SC_BUNDLE_PREFIX` set to the prefix the artifact will be
# installed at. Install somewhere else and pass `--prefix` when building.
#
# Usage
# -----
#   scripts/build-static.sh                          # x86_64, /opt/feldspar
#   scripts/build-static.sh --docker                 # pinned toolchain in a container
#   scripts/build-static.sh --native                 # this machine's toolchain
#   scripts/build-static.sh --target aarch64-unknown-linux-gnu
#   scripts/build-static.sh --prefix /usr/local/feldspar
#   scripts/build-static.sh --no-ui                  # no Node toolchain needed
#   scripts/build-static.sh --release                # ...and publish it to the R2 bucket
#
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
# The upload half of --release, shared with the other platforms' build scripts:
# the bucket, checksums and the uploads (TODO-portable-release D15).
# shellcheck source-path=SCRIPTDIR source=lib/release.sh
source "${REPO_ROOT}/scripts/lib/release.sh"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

TARGET="x86_64-unknown-linux-gnu"
# `auto`: a container when this machine can drive one (the reproducible build),
# this machine's own toolchain when it cannot. Either is a correct artifact; the
# container's is the one whose toolchain is pinned, so it is preferred when it is
# there rather than demanded when it is not.
MODE="auto"
PREFIX="/opt/feldspar"
OUTPUT_DIR="${REPO_ROOT}/dist"
JOBS=""
BUILD_UI=1
STRIP=1
VERIFY=1
RUST_VERSION="1.97"
NODE_VERSION="22.14.0"
RELEASE=0
# The bucket the published artifact lives in (scripts/lib/release.sh).
R2_BUCKET="${DEFAULT_R2_BUCKET}"
# The name the first releases were published under, before names carried a
# platform. Scripts and hosts outside this repository still fetch it, so an
# x86_64 release is published under it too. It has always been an x86_64
# tarball, so only an x86_64 build publishes it.
readonly LEGACY_OBJECT="feldspar.tar.gz"
# The base every build runs on: its glibc (2.36) is the oldest a `linux` binary
# asks of its host (D2).
readonly BUILD_SUITE="bookworm"
readonly BUILD_GLIBC="2.36"
# The bundled CPython: python-build-standalone, pinned by release tag and the
# SHA-256 of each architecture's `install_only_stripped` tarball (D7). Taking a
# CPython security release is a change to these lines. The minor version stays
# 3.13 (D4): environments made by Debian 13's python3.13 keep working, and a
# new minor version is a release of its own.
readonly PBS_RELEASE="20261026"
readonly PBS_PYTHON="3.13.18"
readonly PBS_SHA256_X86_64="df15fe60d3ff55bf9dcf25824b5572756bb8816ff93865a273d3301d7066a8b7"
readonly PBS_SHA256_AARCH64="578d9da1983463ffb0c8c98ab0da2f18177f44069328eb0af847860c1eb05c3e"
readonly PYTHON_VERSION="${PBS_PYTHON%.*}"
readonly PYTHON_LIB="libpython${PYTHON_VERSION}.so.1.0"
# The targets that have a prebuilt V8 archive and a glibc to link statically.
readonly SUPPORTED_TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
)

usage() {
    cat <<EOF
Build the feldspar release tarball: dynamically linked on Debian 12
(${BUILD_SUITE}), with Python ${PBS_PYTHON} bundled. It runs on any glibc >= ${BUILD_GLIBC}
distribution, and on Alpine with the glibc it also bundles.

Usage: scripts/build-static.sh [options]

Target selection
  -t, --target TRIPLE   Rust target triple. Supported:
                          x86_64-unknown-linux-gnu   (default)
                          aarch64-unknown-linux-gnu  (on an aarch64 machine)
                        The musl targets are not supported — V8 has no musl
                        prebuilt — and are rejected with an explanation.

Build environment
      --docker          Build in a container with a pinned toolchain. Needs docker
                        with buildx. The default when both are present, because it
                        is the build that does not depend on what this machine has
                        installed.
      --native          Build with this machine's toolchain — the default when
                        docker/buildx is not available. Needs: rustup target
                        ${TARGET}, clang/libclang, cmake, a C compiler, curl,
                        patchelf and, unless --no-ui, node + npm. It bundles this
                        machine's glibc: built natively anywhere newer than
                        Debian 12, it needs that newer glibc on every host.
      --rust-version V  Rust image tag for --docker (default ${RUST_VERSION}).
      --node-version V  Node version fetched by --docker (default ${NODE_VERSION}).
  -j, --jobs N          Cargo parallelism. Lower it if the linker runs the machine
                        out of memory; this workspace links V8 into the binary.

Packaging
      --prefix PATH     Absolute directory the artifact will be installed to.
                        Compiled into the binary as the bundle location.
                        (default ${PREFIX})
  -o, --output DIR      Where to write the tarball (default ${OUTPUT_DIR}).
      --no-ui           Skip the five front-end bundles (SC_BUILD_ADMIN=0).
                        The server then serves the API only, or a --static-dir
                        you point at a bundle yourself.
      --no-strip        Keep debug symbols (the binary is ~2x larger).
      --no-verify       Skip the post-build checks (linkage, and an install in
                        a container of each supported distribution).

Publishing
      --release         After packaging, upload the tarball and its .sha256 to the
                        Cloudflare R2 bucket ${R2_BUCKET} as
                          ${R2_PUBLIC_BASE}/feldspar-linux-<arch>.tar.gz
                        and, from an x86_64 build, also as
                          ${R2_PUBLIC_BASE}/${LEGACY_OBJECT}
                        (and a .sha256 beside each), then scripts/install.sh as
                        ${R2_PUBLIC_BASE}/install.sh. The names carry no
                        version, so each release overwrites the last. Runs
                        \`npx wrangler r2 object put --remote\`, so it needs npx on
                        PATH and a wrangler login with access to the bucket.
      --bucket NAME     Upload to this bucket instead (default ${R2_BUCKET}).
  -h, --help            This message.
EOF
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

need_value() {
    [[ $# -ge 2 ]] || { echo "error: $1 needs a value" >&2; exit 2; }
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -t|--target)       need_value "$@"; TARGET="$2"; shift 2 ;;
        --docker)          MODE="docker"; shift ;;
        --native)          MODE="native"; shift ;;
        --rust-version)    need_value "$@"; RUST_VERSION="$2"; shift 2 ;;
        --node-version)    need_value "$@"; NODE_VERSION="$2"; shift 2 ;;
        -j|--jobs)         need_value "$@"; JOBS="$2"; shift 2 ;;
        --prefix)          need_value "$@"; PREFIX="$2"; shift 2 ;;
        -o|--output)       need_value "$@"; OUTPUT_DIR="$2"; shift 2 ;;
        --no-ui)           BUILD_UI=0; shift ;;
        --no-strip)        STRIP=0; shift ;;
        --no-verify)       VERIFY=0; shift ;;
        # Removed options, named so a script that still passes one hears why.
        --variant|--deploy|--deploy-variant|--ssh-opt|--remote-tmp)
            echo "error: $1 is gone: there is one artifact now (with Python), and it" \
                 "is installed with its own install.sh or scripts/install.sh" \
                 "(docs/OPERATIONS.md §2)" >&2
            exit 2 ;;
        --release)         RELEASE=1; shift ;;
        --bucket)          need_value "$@"; R2_BUCKET="$2"; shift 2 ;;
        -h|--help)         usage; exit 0 ;;
        *)                 echo "error: unknown option $1" >&2; echo >&2; usage >&2; exit 2 ;;
    esac
done

case "${TARGET}" in
    *-musl)
        cat >&2 <<EOF
error: ${TARGET} is not supported.

  This workspace links V8 (via deno_core), which arrives as rusty_v8's prebuilt
  static archive. Upstream publishes those for *-unknown-linux-gnu, *-apple-darwin
  and Windows — there is no musl archive, and Deno ships no musl build either.
  Building V8 from source against musl is not a supported configuration.

  You do not need a musl target for Alpine: the default
  x86_64-unknown-linux-gnu build carries its own glibc, which its install.sh
  uses on a musl host. Run scripts/build-static.sh with no --target at all.
EOF
        exit 2
        ;;
esac

if [[ ! " ${SUPPORTED_TARGETS[*]} " == *" ${TARGET} "* ]]; then
    echo "error: unsupported target ${TARGET}; supported: ${SUPPORTED_TARGETS[*]}" >&2
    exit 2
fi

# The module runtime's V8 startup snapshot is produced by sc-module's build
# script, which runs on the *host*, and a snapshot is tied to the architecture
# that serialised it. So a build for another architecture is refused here rather
# than half an hour later inside cargo — or, worse, shipped.
host_arch="$(uname -m)"
[[ "${host_arch}" == "arm64" ]] && host_arch="aarch64"
if [[ "${TARGET%%-*}" != "${host_arch}" ]]; then
    cat >&2 <<EOF
error: cannot build ${TARGET} on ${host_arch}.

  The module runtime starts from a V8 startup snapshot, built by
  crates/sc-module/build.rs so that the binary carries its own JavaScript
  instead of reading it from this machine's cargo registry. V8 serialises a
  snapshot for the architecture that made it, and a build script runs on the
  build host — so a ${TARGET} binary made here would carry a ${host_arch}
  snapshot and fail at its first module.

  Build on a ${TARGET%%-*} machine, or use --docker with a matching --platform.
EOF
    exit 2
fi

[[ "${PREFIX}" = /* ]] || { echo "error: --prefix must be absolute, got ${PREFIX}" >&2; exit 2; }

# The architecture as published names spell it.
case "${TARGET}" in
    x86_64-*)  TARGET_ARCH="x86_64" ;;
    aarch64-*) TARGET_ARCH="aarch64" ;;
    *)         TARGET_ARCH="${TARGET%%-*}" ;;
esac

# An empty bucket, an absent npx or a missing installer (scripts/lib/release.sh)
# is worth finding out now and not after the compile.
[[ ${RELEASE} -eq 1 ]] && release_preflight

# A container build needs both docker and its buildx plugin; `docker build` alone
# cannot run the Dockerfile. Deciding here rather than inside `build_docker` keeps
# the mode line below honest about what is going to happen.
if [[ "${MODE}" == "auto" ]]; then
    if command -v docker >/dev/null && docker buildx version >/dev/null 2>&1; then
        MODE="docker"
    else
        MODE="native"
    fi
fi

# The bundled CPython for this target: its download and the SHA-256 it must have.
case "${TARGET}" in
    x86_64-*)  PBS_SHA256="${PBS_SHA256_X86_64}" ;;
    aarch64-*) PBS_SHA256="${PBS_SHA256_AARCH64}" ;;
esac
readonly PBS_SHA256
readonly PBS_URL="https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_RELEASE}/cpython-${PBS_PYTHON}+${PBS_RELEASE}-${TARGET}-install_only_stripped.tar.gz"
readonly PYTHON_BUNDLE="${REPO_ROOT}/scripts/lib/python-bundle.sh"

# A native build links and bundles *this* machine's glibc, so the floor it puts
# on every host is this machine's glibc, not bookworm's. Still a correct
# artifact for hosts at least that new, so a warning; the tools it cannot work
# without are errors, before the build rather than after it.
if [[ "${MODE}" == "native" ]]; then
    for tool in curl patchelf ldd sha256sum; do
        command -v "${tool}" >/dev/null ||
            { echo "error: a --native build needs ${tool} here (or --docker)" >&2; exit 1; }
    done
    found_glibc="$(ldd --version 2>/dev/null | sed -n '1s/.* \([0-9][0-9.]*\)$/\1/p')"
    if [[ -n "${found_glibc}" && "${found_glibc}" != "${BUILD_GLIBC}" ]]; then
        echo "warning: this machine has glibc ${found_glibc}; a native build needs" \
             "that on every host, where the --docker build needs ${BUILD_GLIBC}" >&2
    fi
fi

# ---------------------------------------------------------------------------
# Naming
# ---------------------------------------------------------------------------

# The workspace version, read from the one place it is declared.
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' \
    "${REPO_ROOT}/Cargo.toml" | head -n1)"
[[ -n "${VERSION}" ]] || { echo "error: could not read the version from Cargo.toml" >&2; exit 1; }

# A dirty tree is worth recording in the artifact's name: an operator holding two
# tarballs should be able to tell which one came from a checkout with edits in it.
GIT_DESC=""
if git -C "${REPO_ROOT}" rev-parse --git-dir >/dev/null 2>&1; then
    GIT_DESC="$(git -C "${REPO_ROOT}" rev-parse --short HEAD)"
    git -C "${REPO_ROOT}" diff --quiet HEAD 2>/dev/null || GIT_DESC="${GIT_DESC}-dirty"
fi

# The build's identity, compiled into the binary and shown under Settings → System
# (`crates/sc-server/src/system.rs`), so "which version is installed?" has an
# answer on the server itself. Both are read with `option_env!`: a binary built
# any other way has neither and says it is not a release build. The commit is the
# full hash, `-dirty` as above; the time is when this script ran, in UTC.
SC_BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
SC_BUILD_COMMIT=""
if [[ -n "${GIT_DESC}" ]]; then
    SC_BUILD_COMMIT="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
    [[ "${GIT_DESC}" == *-dirty ]] && SC_BUILD_COMMIT="${SC_BUILD_COMMIT}-dirty"
fi
export SC_BUILD_TIME SC_BUILD_COMMIT

NAME="feldspar-${VERSION}${GIT_DESC:+-${GIT_DESC}}-${TARGET}"
STAGE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/feldspar-package.XXXXXX")"
trap 'rm -rf "${STAGE_ROOT}"' EXIT
STAGE="${STAGE_ROOT}/tree"
BINARY="${STAGE}/bin/feldspar"
TARBALL="${OUTPUT_DIR}/${NAME}.tar.gz"
mkdir -p "${STAGE}"
# Every name the tarball is published under; the first is the one a new host
# is pointed at.
OBJECTS=("feldspar-linux-${TARGET_ARCH}.tar.gz")
[[ "${TARGET_ARCH}" == x86_64 ]] && OBJECTS+=("${LEGACY_OBJECT}")
OBJECT="${OBJECTS[0]}"

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

log "feldspar ${VERSION}${GIT_DESC:+ (${GIT_DESC})}, built ${SC_BUILD_TIME}"
log "target ${TARGET}, ${MODE} build, install prefix ${PREFIX}"
[[ "${MODE}" == "native" ]] && ! command -v docker >/dev/null &&
    log "note: no docker/buildx here, so this is a build with the local toolchain"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

build_docker() {
    command -v docker >/dev/null || {
        echo "error: docker not found; use --native to build with this machine's toolchain" >&2
        exit 1
    }
    # The Dockerfile is BuildKit-only — cache mounts and a `scratch` export stage
    # written straight to the host — and the legacy builder fails on it with an
    # error that does not say so. Checked here, where the answer can.
    docker buildx version >/dev/null 2>&1 || {
        cat >&2 <<'MSG'
error: docker buildx is not available, and this build needs it (the Dockerfile
       uses BuildKit cache mounts and a filesystem export stage).

  Install it — on Debian/Ubuntu, `sudo apt install docker-buildx-plugin`; on other
  systems see https://docs.docker.com/go/buildx/ — or build with this machine's
  own toolchain instead:

      scripts/build-static.sh --native
MSG
        exit 1
    }

    # The container's architecture has to be the target's: the toolchain inside is
    # a native one, not a cross toolchain. On a foreign architecture this runs
    # under qemu (binfmt) and is slow but correct.
    local platform
    case "${TARGET}" in
        x86_64-*)  platform="linux/amd64" ;;
        aarch64-*) platform="linux/arm64" ;;
    esac

    log "building in docker (${platform}, ${BUILD_SUITE}); the first build compiles ~1000 crates and V8's bindings"
    DOCKER_BUILDKIT=1 docker build \
        --platform "${platform}" \
        --file "${REPO_ROOT}/scripts/static-build.Dockerfile" \
        --target export \
        --output "type=local,dest=${STAGE}" \
        --build-arg "RUST_VERSION=${RUST_VERSION}" \
        --build-arg "DEBIAN_SUITE=${BUILD_SUITE}" \
        --build-arg "PBS_URL=${PBS_URL}" \
        --build-arg "PBS_SHA256=${PBS_SHA256}" \
        --build-arg "STRIP=${STRIP}" \
        --build-arg "NODE_VERSION=${NODE_VERSION}" \
        --build-arg "TARGET=${TARGET}" \
        --build-arg "PREFIX=${PREFIX}" \
        --build-arg "SC_BUILD_ADMIN=${BUILD_UI}" \
        --build-arg "SC_BUILD_TIME=${SC_BUILD_TIME}" \
        --build-arg "SC_BUILD_COMMIT=${SC_BUILD_COMMIT}" \
        --build-arg "JOBS=${JOBS}" \
        "${REPO_ROOT}"
}

build_native() {
    command -v cargo >/dev/null || { echo "error: cargo not found" >&2; exit 1; }
    if ! rustup target list --installed 2>/dev/null | grep -qx "${TARGET}"; then
        log "adding rust target ${TARGET}"
        rustup target add "${TARGET}"
    fi
    if [[ ${BUILD_UI} -eq 1 ]] && ! command -v npm >/dev/null; then
        echo "error: npm not found; install node, or pass --no-ui" >&2
        exit 1
    fi

    log "building with this machine's toolchain"
    # The bundled CPython first, straight into the tree that ships: a download
    # that does not match the pin stops the build before it starts. Its
    # `sysconfig` names ${PREFIX}/python, where it will be installed, which is
    # not where it is here — so PyO3 is configured by a file that names this
    # copy, rather than by asking the interpreter.
    sh "${PYTHON_BUNDLE}" fetch "${PBS_URL}" "${PBS_SHA256}" "${STAGE}" "${PREFIX}/python"
    local pyo3_config="${STAGE_ROOT}/pyo3-config.txt"
    cat > "${pyo3_config}" <<EOF
implementation=CPython
version=${PYTHON_VERSION}
shared=true
abi3=false
lib_name=python${PYTHON_VERSION}
lib_dir=${STAGE}/python/lib
executable=${STAGE}/python/bin/python3
pointer_width=64
build_flags=
suppress_build_script_link_lines=false
EOF
    # Its own target directory, where it has always been: different RUSTFLAGS
    # and features from a developer's `cargo build`, so sharing that one would
    # rebuild the whole tree on every switch.
    local target_dir="${REPO_ROOT}/target/linux-python"
    (
        cd "${REPO_ROOT}"
        # `SC_BUNDLE_PREFIX` re-roots the bundle paths compiled into the binary
        # at the install prefix: the bundles are built here, but the binary is
        # going somewhere this checkout does not exist.
        SC_BUNDLE_PREFIX="${PREFIX}" \
        SC_BUILD_ADMIN="${BUILD_UI}" \
        CARGO_TARGET_DIR="${target_dir}" \
        PYO3_CONFIG_FILE="${pyo3_config}" \
        RUSTFLAGS="-C link-arg=-Wl,-rpath,\$ORIGIN/../python/lib${RUSTFLAGS:+ ${RUSTFLAGS}}" \
            cargo build --release --target "${TARGET}" -p sc-cli --features python \
                ${JOBS:+--jobs "${JOBS}"}
    )
    local built="${target_dir}/${TARGET}/release/feldspar"

    mkdir -p "${STAGE}/bin"
    cp "${built}" "${STAGE}/bin/feldspar"
    # Stripped (or not) by the bundle step, because the `.glibc` copy it makes
    # has to be a copy of the binary that ships.
    sh "${PYTHON_BUNDLE}" glibc "${STAGE}" "${PREFIX}" "${STRIP}"
    stage_plugins
    if [[ ${BUILD_UI} -eq 1 ]]; then
        # Every bundle `crates/sc-cli/build.rs` builds (its `BUNDLES`), because the
        # binary carries each one's path under the prefix: one missing here is a
        # route that answers "not built" on the installed server
        # (`tests/build_script.rs` holds this list to that one).
        for bundle in admin ide saltcorn-ui builder analytics; do
            local dist="${REPO_ROOT}/ui/${bundle}/dist"
            [[ -d "${dist}" ]] || { echo "error: ${dist} was not built" >&2; exit 1; }
            mkdir -p "${STAGE}/ui/${bundle}"
            cp -r "${dist}" "${STAGE}/ui/${bundle}/"
        done
    fi
}

# The **bundled modules**: source, and deliberately no dependency tree. What
# ships is `plugins/<id>/` — an `index.js` and a `package.json`, or a
# `pyproject.toml` and a package — and npm or pip fetches what it needs at the
# moment an admin installs one (`crates/sc-module/src/bundled.rs`). So this is a
# copy of a few kilobytes and not a vendored `node_modules`, which is the whole
# point: a server that installs none of them downloads nothing.
#
# `--no-ui` does not turn this off. That flag means "this machine has no Node
# toolchain to build the SPA with", and there is nothing here to build.
stage_plugins() {
    local plugins="${REPO_ROOT}/plugins"
    [[ -d "${plugins}" ]] || { echo "error: ${plugins} is missing; it belongs in the artifact" >&2; exit 1; }
    mkdir -p "${STAGE}/plugins"
    cp -r "${plugins}/." "${STAGE}/plugins/"
    # Nothing a package manager left behind travels: a `node_modules` from a
    # developer running `npm install` in a plugin directory would be exactly the
    # tree this design does not ship.
    find "${STAGE}/plugins" -maxdepth 2 \( -name node_modules -o -name __pycache__ -o -name '*.egg-info' -o -name dist -o -name build \) \
        -exec rm -rf {} + 2>/dev/null || true
    # Nor does a fetched test corpus (`plugins/imap/test/corpus/`, git-ignored: GPL files and
    # other people's mail that must not be redistributed), a `node_modules` deeper down, or the
    # Dovecot the mail tests unpack and run (`test/servers/run/`, the spike's `run/`).
    find "${STAGE}/plugins" \( -name node_modules -o -path '*/test/corpus' -o -path '*/test/servers/run' -o -path '*/spike/run' \) \
        -prune -exec rm -rf {} + 2>/dev/null || true
}

# Build the tree into ${STAGE}. It is stripped where its `.glibc` copy is made,
# in the container or by the bundle step: stripping only one of the pair here
# would ship two different binaries.
build_tree() {
    case "${MODE}" in
        docker) build_docker ;;
        native) build_native ;;
    esac

    [[ -f "${BINARY}" ]] || { echo "error: the build produced no binary at ${BINARY}" >&2; exit 1; }
    chmod +x "${BINARY}"
}

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

# `readelf -d FILE`'s entries of one tag (NEEDED, RPATH, RUNPATH), one per line.
dynamic_entries() {
    readelf -d "$1" | sed -n "s/.*(${2}).*\[\(.*\)\].*/\1/p"
}

# The binary's linkage: dynamic, asking for exactly the libpython the tree carries, and finding it in
# the tree — through `$ORIGIN/../python/lib`, never through the host's library
# path, where a different CPython's libpython may be (TODO-portable-release §1).
assert_python_linkage() {
    command -v readelf >/dev/null ||
        { echo "error: checking the artifact needs readelf (binutils), or --no-verify" >&2; exit 1; }
    local described needed runpath
    described="$(file -b "${BINARY}")"
    case "${described}" in
        *"dynamically linked"*) ;;
        *)
            echo "error: the binary is not dynamically linked:" >&2
            echo "  ${described}" >&2
            exit 1
            ;;
    esac
    needed="$(dynamic_entries "${BINARY}" NEEDED)"
    if ! grep -qx "${PYTHON_LIB}" <<<"${needed}"; then
        echo "error: the binary does not link ${PYTHON_LIB}; it needs:" >&2
        while IFS= read -r lib; do printf '  %s\n' "${lib}"; done <<<"${needed}" >&2
        exit 1
    fi
    runpath="$(dynamic_entries "${BINARY}" RUNPATH; dynamic_entries "${BINARY}" RPATH)"
    # shellcheck disable=SC2016  # the literal `$ORIGIN` the linker reads, not a variable
    if ! tr ':' '\n' <<<"${runpath}" | grep -qx '$ORIGIN/../python/lib'; then
        echo "error: the binary does not look for ${PYTHON_LIB} beside itself" \
             "(\$ORIGIN/../python/lib); its search path is: ${runpath:-none}" >&2
        exit 1
    fi
    [[ -f "${STAGE}/python/lib/${PYTHON_LIB}" ]] ||
        { echo "error: the tree has no python/lib/${PYTHON_LIB} for the binary to find" >&2; exit 1; }
    log "linkage: dynamic, needs $(tr '\n' ' ' <<<"${needed}")from \$ORIGIN/../python/lib — $(du -h "${BINARY}" | cut -f1)"
}

# The `.glibc` copies (D3): each one's loader is the bundled one, at its
# installed path, and its search path is a DT_RPATH — which glibc applies to
# everything the process loads, NSS modules and `ctypes` included, where a
# RUNPATH would reach only the executable's own NEEDED. And every library the
# tree's ELF files ask for is in the tree, so a host with no glibc of its own
# has nothing left to provide.
assert_glibc_copies() {
    local loader
    case "${TARGET}" in
        x86_64-*)  loader="ld-linux-x86-64.so.2" ;;
        aarch64-*) loader="ld-linux-aarch64.so.1" ;;
    esac
    local want_interp="${PREFIX}/glibc/${loader}"
    local want_rpath="${PREFIX}/glibc:${PREFIX}/python/lib"
    [[ -f "${STAGE}/glibc/${loader}" && -f "${STAGE}/glibc/libc.so.6" ]] ||
        { echo "error: the tree's glibc/ has no ${loader} and libc.so.6" >&2; exit 1; }
    local copy interp rpath runpath
    for copy in "${STAGE}/bin/feldspar.glibc" "${STAGE}/python/bin/python3.glibc"; do
        [[ -f "${copy}" ]] || { echo "error: ${copy#"${STAGE}/"} is missing" >&2; exit 1; }
        interp="$(readelf -l "${copy}" | sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p')"
        rpath="$(dynamic_entries "${copy}" RPATH)"
        runpath="$(dynamic_entries "${copy}" RUNPATH)"
        if [[ "${interp}" != "${want_interp}" || "${rpath}" != "${want_rpath}" || -n "${runpath}" ]]; then
            cat >&2 <<EOF
error: ${copy#"${STAGE}/"} is not set up to run on the bundled glibc:
  loader   ${interp:-none}   (want ${want_interp})
  RPATH    ${rpath:-none}   (want ${want_rpath})
  RUNPATH  ${runpath:-none}   (want none)
EOF
            exit 1
        fi
    done
    local elf name missing=()
    while IFS= read -r elf; do
        while IFS= read -r name; do
            # shellcheck disable=SC2016  # the literal `$ORIGIN`, as readelf prints it
            case "${name}" in '$ORIGIN'*|libpython3.so) continue ;; esac
            [[ -e "${STAGE}/glibc/${name}" || -e "${STAGE}/python/lib/${name}" ]] ||
                missing+=("${elf#"${STAGE}/"} needs ${name}")
        done < <(dynamic_entries "${elf}" NEEDED)
    done < <(find "${STAGE}/bin" "${STAGE}/python/bin" "${STAGE}/python/lib" "${STAGE}/glibc" \
                 -type f \( -name '*.so*' -o -perm -u+x \) -exec sh -c \
                 'head -c4 "$1" | grep -q "ELF" && echo "$1"' _ {} \;)
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "error: the bundled glibc is incomplete:" >&2
        printf '  %s\n' "${missing[@]}" >&2
        exit 1
    fi
    log "glibc copies: loader ${want_interp}, RPATH ${want_rpath}; $(find "${STAGE}/glibc" -type f | wc -l) libraries in glibc/"
}

# The claim in this script's header, executed: the artifact installs and runs on
# every distribution it is for. Skipped when the target is not this machine's
# architecture (nothing here would run it) or docker is absent. It runs after
# packaging, because what is tested is the tarball.
#
# The tarball is installed in each of SMOKE_IMAGES by the curl installer from a
# file:// release, and `scripts/lib/smoke-linux.sh` then runs the self-check
# (with its HTTPS request), checks which libc the process maps, and on Debian 13
# and Alpine makes a venv, pip-installs a pure-Python package and a manylinux
# wheel and imports both, and upgrades a running install by swapping trees
# (TODO-portable-release R2.4, R2.6). Derivatives (Mint, MX, CachyOS,
# Manjaro, EndeavourOS, Rocky) are covered by ID_LIKE, not by images.
readonly SMOKE_IMAGES=(
    debian:13-slim debian:12-slim ubuntu:24.04 ubuntu:latest
    fedora:latest almalinux:10 archlinux:latest
    opensuse/leap:16.0 opensuse/tumbleweed alpine:3
)
readonly SMOKE_SCRIPT="${REPO_ROOT}/scripts/lib/smoke-linux.sh"

smoke_test_distros() {
    command -v docker >/dev/null || { log "skipping distro smoke test (no docker)"; return 0; }
    local host_arch
    host_arch="$(uname -m)"
    case "${TARGET}" in
        x86_64-*)  [[ "${host_arch}" == "x86_64"  ]] || { log "skipping distro smoke test (foreign architecture)"; return 0; } ;;
        aarch64-*) [[ "${host_arch}" == "aarch64" ]] || { log "skipping distro smoke test (foreign architecture)"; return 0; } ;;
    esac

    # The output is held rather than streamed — a page of text per image —
    # and shown when a run fails, or for the lines that say what it found.
    local image output
    # The release as the bucket holds it: the object name the curl
        # installer fetches, and a checksum file naming that object.
    local release="${STAGE_ROOT}/smoke-release"
    rm -rf "${release}"
    mkdir -p "${release}"
    cp "${TARBALL}" "${release}/${OBJECT}"
    printf '%s  %s\n' "$(cut -d' ' -f1 < "${TARBALL}.sha256")" "${OBJECT}" \
        > "${release}/${OBJECT}.sha256"
    local extra
    for image in "${SMOKE_IMAGES[@]}"; do
        extra=()
        case "${image}" in debian:13-slim|alpine:3) extra=(--venv --upgrade) ;; esac
        log "smoke test on ${image}${extra[*]:+ (${extra[*]})}"
        if ! output="$(docker run --rm \
                -v "${release}:/release:ro" \
                -v "${REPO_ROOT}/scripts/install.sh:/install.sh:ro" \
                -v "${SMOKE_SCRIPT}:/smoke.sh:ro" \
                "${image}" sh /smoke.sh /release "${OBJECT}" "${PREFIX}" \
                ${extra[@]+"${extra[@]}"} 2>&1)"; then
            echo "error: the artifact failed its smoke test on ${image}:" >&2
            printf '%s\n' "${output}" >&2
            exit 1
        fi
        grep -E '^smoke: (libc choice|maps|NEEDS)' <<<"${output}" | sed 's/^/    /' || true
    done
}

verify_tree() {
    assert_python_linkage
    assert_glibc_copies
}

# ---------------------------------------------------------------------------
# Package
# ---------------------------------------------------------------------------

readonly INSTALL_TREE="${REPO_ROOT}/scripts/lib/install-tree.sh"

package_tree() {
[[ -f "${INSTALL_TREE}" ]] ||
    { echo "error: ${INSTALL_TREE} is missing; it belongs in the artifact" >&2; exit 1; }

# Which artifact this is, installed beside the binary, for an operator and for
# the curl installer. There has been only one kind since the static artifact was
# retired; older trees say `static` or `debian13-python`.
echo "linux" > "${STAGE}/VARIANT"

# The tree's installer (scripts/lib/install-tree.sh): it swaps the whole prefix
# in by rename rather than copying over a running server's files, chooses the
# libc, and stops and starts running feldspar units around the swap. The prefix
# it installs to, and the glibc the binary was linked against, are this build's.
sed -e "s|@PREFIX@|${PREFIX}|g" -e "s|@BUILD_GLIBC@|${BUILD_GLIBC}|g" \
    "${INSTALL_TREE}" > "${STAGE}/install.sh"
chmod +x "${STAGE}/install.sh"

cat > "${STAGE}/README" <<EOF
feldspar ${VERSION}${GIT_DESC:+ (${GIT_DESC})} — ${TARGET}

This binary is dynamically linked, built on Debian 12, and carries its own
CPython ${PBS_PYTHON} (python-build-standalone, ${PBS_RELEASE}) in python/, which it
finds beside itself: it runs Python trigger bodies and installs Python modules
with nothing Python-related on the host, and pip runs that same interpreter.

From the host it needs glibc ${BUILD_GLIBC} or newer, which Debian 12+, Ubuntu
24.04+, Fedora, RHEL 10, Arch and openSUSE all have. For a musl host (Alpine)
the tarball also carries Debian 12's glibc in glibc/, and bin/feldspar.glibc
and python/bin/python3.glibc: the same programs, set to run on it.
\`${PREFIX}/bin/feldspar python check\` says which interpreter started and
whether it can import ssl, sqlite3, ctypes and zoneinfo and reach PyPI.

Install
  sudo ./install.sh
      installs this tree as ${PREFIX} and links feldspar onto PATH: copied
      beside it, then swapped in by rename, so a running server is never
      written over. Running feldspar units are stopped for the swap and
      started again (--no-restart leaves them alone), and the previous tree is
      kept as ${PREFIX}.old.
  sudo feldspar setup --server --domain example.com
      makes this host a server, once: PostgreSQL, Node.js, a headless browser,
      the feldspar service account, /etc/feldspar/feldspar.toml and the
      service. --dry-run prints what it would do; --help lists the options.

On a host that already runs feldspar, the first command is the whole upgrade.
Take a database dump first (docs/OPERATIONS.md §3); install.sh prints the
command that puts the previous tree back.

Where a newer one comes from
  ${R2_PUBLIC_BASE}/${OBJECT}
  ${R2_PUBLIC_BASE}/${OBJECT}.sha256
  That URL is always the latest build; it carries no version, so there is nothing
  to look up. \`sha256sum -c ${OBJECT}.sha256\` checks a download.

What is in the tarball
  bin/feldspar          the server and management CLI
  bin/feldspar.glibc    the same, set to run on glibc/ (for a musl host);
                        install.sh moves it over bin/feldspar where the host's
                        glibc cannot run that, and says so (LIBC records it)
  python/               CPython ${PBS_PYTHON}: what bin/feldspar embeds and pip runs
  glibc/                Debian 12's glibc, used only by the .glibc copies
  install.sh            the installer above
  plugins/              the modules this release ships with. They are not
                        loaded until somebody installs one from Settings ->
                        Modules, which is one click; what each depends on is
                        fetched from npm or PyPI at that moment and is not in
                        this tarball.
$(if [[ ${BUILD_UI} -eq 1 ]]; then
cat <<INNER
  ui/admin/dist         the admin SPA, served by \`feldspar serve\`
  ui/ide/dist           the file-store IDE, reached from the admin UI
  ui/saltcorn-ui/dist   Saltcorn UI: the view runtime and browser assets that
                        serve Saltcorn 1 views
  ui/builder/dist       Saltcorn UI's builder: Saltcorn 1's drag-and-drop layout
                        editor, served on the admin server's builder routes
  ui/analytics/dist     the Analytics UI: workspaces over datasets, models and
                        panels, reached from the admin UI's sidebar (/analytics/)
INNER
else
cat <<INNER
  (built with --no-ui: no admin SPA, no IDE, no builder and no Analytics UI. \`serve\` will answer the API and
  serve the bootstrap page; point --static-dir at a bundle to serve one.)
INNER
fi)

The bundle paths are compiled into the binary as ${PREFIX}/ui/<name>/dist, which
is why the tree has to be installed at ${PREFIX}. --static-dir overrides the admin
bundle at run time; the IDE's and Saltcorn UI's paths have no flag.

Limits
  Hostnames are resolved by the binary's own resolver (/etc/resolv.conf and
  /etc/hosts), not by glibc's NSS. /etc/nsswitch.conf does not affect this
  server: a name it must reach belongs in DNS or in /etc/hosts.
EOF

mkdir -p "${OUTPUT_DIR}"

# Packaged with the release name as the top-level directory, so unpacking in a
# scratch directory does not scatter `bin/` and `ui/` over whatever is there.
# Owned by root in the archive, not by whoever built it: install.sh keeps
# what it copies as it finds it (`cp -p`, for the .pyc timestamps), and a tree
# unpacked as root would otherwise belong to the build account's uid.
tar -czf "${TARBALL}" -C "${STAGE}" --owner=0 --group=0 --numeric-owner \
    --transform "s,^\.,${NAME}," .
release_checksum "${TARBALL}"

log "wrote ${TARBALL} ($(du -h "${TARBALL}" | cut -f1))"
}

build_tree
[[ ${VERIFY} -eq 1 ]] && verify_tree
package_tree
[[ ${VERIFY} -eq 1 ]] && smoke_test_distros

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

# Copy the artifact to the R2 bucket under each of its fixed, version-less
# names, so that an installer can always fetch the same URL and get the newest
# build; then the curl installer, last.
if [[ ${RELEASE} -eq 1 ]]; then
    release_warn_if_dirty "${GIT_DESC}"
    release_upload "${TARBALL}" "${STAGE_ROOT}" "${OBJECTS[@]}"
    if release_is_public; then
        cat <<EOF

It is now what a new host downloads:

  curl -fLO ${R2_PUBLIC_BASE}/${OBJECT}
  curl -fLO ${R2_PUBLIC_BASE}/${OBJECT}.sha256
EOF
        for older in "${OBJECTS[@]:1}"; do
            echo
            echo "It is also what a host that fetches its older name gets: ${R2_PUBLIC_BASE}/${older}"
        done
    fi
    release_upload_installer
else
    cat <<EOF

To install it on another machine of the same architecture:

  scp ${TARBALL} host:/tmp/
  ssh -t host 'tar -xzf /tmp/${NAME}.tar.gz -C /tmp && sudo /tmp/${NAME}/install.sh'
  ssh -t host 'sudo feldspar setup --server --domain example.com'   # a new host only
EOF
fi
