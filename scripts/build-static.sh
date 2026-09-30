#!/usr/bin/env bash
#
# Build a self-contained `feldspar` release that can be copied to another VM.
#
# What "static" means here, and why
# ---------------------------------
# The artifact is a **statically linked glibc binary** (`x86_64-unknown-linux-gnu`
# with `-C target-feature=+crt-static`, which rustc emits as a static-PIE). One
# file, no shared-library dependencies, no interpreter — so it runs on Debian and
# on Alpine from the same tarball, which is what this is for.
#
# The obvious way to get an Alpine-compatible binary is a musl target, and that
# way is closed: `deno_core` links **V8**, and V8 reaches this build as
# `rusty_v8`'s prebuilt static archive, which upstream publishes for
# `*-unknown-linux-gnu`, `*-apple-darwin` and Windows only — there is no musl
# archive, and Deno itself ships no musl build for the same reason. Building V8
# from source (`V8_FROM_SOURCE=1`) is hours of GN/ninja against a glibc sysroot,
# unsupported on musl, and would have to be repeated for every V8 bump. So the
# musl targets are refused by name below, with that message, rather than failing
# forty minutes into a build.
#
# Static glibc has one well-known sharp edge — `getaddrinfo` and NSS — and this
# build does not rely on getting it right. `nss_files` and `nss_dns` have been
# inside libc since glibc 2.34, but any *other* module on the `hosts:` line of
# /etc/nsswitch.conf is still a `dlopen` of a shared object linked against the
# shared glibc, which loads a second complete `libc.so.6` into this binary and
# kills it. Debian and Ubuntu put `myhostname` on that line by default, so this
# is the common case, not the exotic one — and it is invisible until the process
# resolves its first hostname, which for this server is the ACME CA, seconds
# after TLS is switched on. `crates/sc-dns` therefore resolves names in Rust and
# `crates/sc-cli/build.rs` links it in front of glibc's resolver; nothing here
# calls NSS. See README §4.1.
#
# The residual limit is `dlopen`, which a static binary cannot do: native Node
# addons (`.node` files loaded through `deno_napi`) will not load. Nothing in the
# server needs one; an application that ships one does.
#
# What it produces
# ----------------
#   dist/feldspar-<version>-<target>.tar.gz   the artifact
#   dist/feldspar-<version>-<target>.tar.gz.sha256
#
# unpacking to the **install prefix** (`/opt/feldspar` by default):
#
#   /opt/feldspar/bin/feldspar        the binary
#   /opt/feldspar/ui/admin/dist       the admin SPA it serves
#   /opt/feldspar/ui/ide/dist         the file-store IDE it serves
#   /opt/feldspar/ui/saltcorn-ui/dist Saltcorn UI's view runtime and assets
#   /opt/feldspar/ui/builder/dist     Saltcorn UI's builder, the layout editor
#   /opt/feldspar/ui/analytics/dist   the Analytics UI, served under /analytics/
#   /opt/feldspar/plugins/            the modules it ships with, installable in
#                                     one click from Settings -> Modules
#   /opt/feldspar/install.sh          copies the tree into place
#   /opt/feldspar/setup-host.sh       sets the host up to run it (packages,
#                                     database, config file, systemd unit)
#
# so that a machine with nothing on it needs three commands and no checkout:
# unpack, `./install.sh`, `/opt/feldspar/setup-host.sh`.
#
# The prefix is not cosmetic: `crates/sc-cli/build.rs` compiles the two bundle
# paths into the binary, and the IDE's has no run-time flag to override it. This
# script therefore builds with `SC_BUNDLE_PREFIX` set to the prefix the artifact
# will be installed at, so the paths inside the binary describe the target
# machine. Install somewhere else and pass `--prefix` when building.
#
# Usage
# -----
#   scripts/build-static.sh                          # x86_64, /opt/feldspar
#   scripts/build-static.sh --docker                 # pinned toolchain in a container
#   scripts/build-static.sh --native                 # this machine's toolchain
#   scripts/build-static.sh --target aarch64-unknown-linux-gnu
#   scripts/build-static.sh --prefix /usr/local/feldspar
#   scripts/build-static.sh --no-ui                  # no Node toolchain needed
#   scripts/build-static.sh --deploy root@vm         # ...and install it there over ssh
#   scripts/build-static.sh --release                # ...and publish it to the R2 bucket
#
set -euo pipefail

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

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
DEPLOY_HOST=""
REMOTE_TMP="/tmp"
SSH_OPTS=()
RELEASE=0
# The bucket the published artifact lives in. The objects inside it are named
# without a version — `feldspar.tar.gz` — because they are the *latest* build:
# whatever fetches them wants the current one and has no version to ask for.
readonly DEFAULT_R2_BUCKET="feldspar-latest-static"
R2_BUCKET="${DEFAULT_R2_BUCKET}"
R2_OBJECT="feldspar.tar.gz"
# What that bucket is served as, and what the README tells an operator to
# download. Printed after a publish so that what was uploaded and what people
# fetch are visibly the same thing. A --bucket somewhere else has no such URL and
# is not given one.
readonly R2_PUBLIC_BASE="https://feldspar-latest-static.saltcorn.com"

# The targets that have a prebuilt V8 archive and a glibc to link statically.
readonly SUPPORTED_TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
)

usage() {
    cat <<EOF
Build a self-contained, statically linked feldspar binary and package it for
installation on another machine.

Usage: scripts/build-static.sh [options]

Target selection
  -t, --target TRIPLE   Rust target triple. Supported:
                          x86_64-unknown-linux-gnu   (default)
                          aarch64-unknown-linux-gnu
                        Both are built with +crt-static, which produces a binary
                        with no dynamic dependencies: it runs on glibc distributions
                        (Debian, Ubuntu, RHEL, ...) and on musl ones (Alpine) alike.
                        The musl *targets* are not supported — V8 has no musl
                        prebuilt — and are rejected with an explanation.

Build environment
      --docker          Build in a container with a pinned toolchain. Needs docker
                        with buildx. The default when both are present, because it
                        is the build that does not depend on what this machine has
                        installed.
      --native          Build with this machine's toolchain — the default when
                        docker/buildx is not available. Needs: rustup target
                        ${TARGET}, clang/libclang, cmake, a C compiler, libz.a
                        (zlib1g-dev) and, unless --no-ui, node + npm.
      --rust-version V  Rust image tag for --docker (default ${RUST_VERSION}).
      --node-version V  Node version fetched by --docker (default ${NODE_VERSION}).
  -j, --jobs N          Cargo parallelism. Lower it if the linker runs the machine
                        out of memory; this workspace links V8 into the binary.

Packaging
      --prefix PATH     Absolute directory the artifact will be installed to.
                        Compiled into the binary as the admin/IDE bundle location.
                        (default ${PREFIX})
  -o, --output DIR      Where to write the tarball (default ${OUTPUT_DIR}).
      --no-ui           Skip the five front-end bundles (SC_BUILD_ADMIN=0).
                        The server then serves the API only, or a --static-dir
                        you point at a bundle yourself.
      --no-strip        Keep debug symbols (the binary is ~2x larger).
      --no-verify       Skip the post-build checks (static linkage, and a smoke
                        run on Debian and Alpine containers).

Deployment
      --deploy [USER@]HOST
                        After packaging, copy the tarball to HOST over ssh, unpack
                        it there and run its install.sh, so the artifact ends up
                        installed at ${PREFIX} on that machine. HOST is anything
                        ssh accepts, a ~/.ssh/config alias included. The remote
                        side runs install.sh under sudo unless the login is root.
                        A running feldspar.service is stopped just before the
                        install and started again after it; a unit that is absent
                        or already stopped is left alone.
      --ssh-opt OPT     Extra option for the ssh invocations, e.g. --ssh-opt -p2222
                        or --ssh-opt -oStrictHostKeyChecking=no. Repeatable; each
                        occurrence is one argv element, so write a flag and its
                        value together (-p2222, not -p 2222).
      --remote-tmp DIR  Directory on the remote to copy and unpack into
                        (default ${REMOTE_TMP}). Removed again afterwards.

Publishing
      --release         After packaging, upload the tarball and its .sha256 to the
                        Cloudflare R2 bucket ${R2_BUCKET}, as
                        ${R2_OBJECT} and ${R2_OBJECT}.sha256 — the object names
                        carry no version, so each release overwrites the last and
                        the download URL never changes:
                          ${R2_PUBLIC_BASE}/${R2_OBJECT}
                          ${R2_PUBLIC_BASE}/${R2_OBJECT}.sha256
                        Runs
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
        --deploy)          need_value "$@"; DEPLOY_HOST="$2"; shift 2 ;;
        --ssh-opt)         need_value "$@"; SSH_OPTS+=("$2"); shift 2 ;;
        --remote-tmp)      need_value "$@"; REMOTE_TMP="$2"; shift 2 ;;
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
  x86_64-unknown-linux-gnu build is linked with +crt-static, so it has no
  interpreter and no shared libraries, and runs on Alpine unchanged. Run
  scripts/build-static.sh with no --target at all.
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

# Everything about --deploy that can be known before the build is checked before
# the build: an option that cannot work, or a missing ssh, is otherwise found out
# at the end of an hour of compiling V8, with the artifact built and nowhere to
# put it.
if [[ -n "${DEPLOY_HOST}" ]]; then
    [[ "${REMOTE_TMP}" = /* ]] ||
        { echo "error: --remote-tmp must be absolute, got ${REMOTE_TMP}" >&2; exit 2; }
    command -v ssh >/dev/null ||
        { echo "error: --deploy needs ssh, which is not on PATH" >&2; exit 1; }
fi

# Likewise for --release: an absent npx, or a wrangler that cannot see the bucket,
# is worth finding out now and not after the compile. The bucket listing is a
# warning rather than an error — it costs a network round trip that can fail for
# reasons that have nothing to do with the upload — but a missing npx cannot work
# at all.
if [[ ${RELEASE} -eq 1 ]]; then
    [[ -n "${R2_BUCKET}" ]] || { echo "error: --bucket must not be empty" >&2; exit 2; }
    command -v npx >/dev/null ||
        { echo "error: --release needs npx (node), which is not on PATH" >&2; exit 1; }
fi

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

NAME="feldspar-${VERSION}${GIT_DESC:+-${GIT_DESC}}-${TARGET}"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/feldspar-package.XXXXXX")"
trap 'rm -rf "${STAGE}"' EXIT

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

log "feldspar ${VERSION}${GIT_DESC:+ (${GIT_DESC})}"
log "target ${TARGET}, ${MODE} build, install prefix ${PREFIX}"
[[ "${MODE}" == "native" ]] && ! command -v docker >/dev/null &&
    log "note: no docker/buildx here, so this is a build with the local toolchain"

# `${SSH_OPTS[@]}` on its own would be an unbound variable under `set -u` when no
# --ssh-opt was given, on bash before 4.4; this expansion is empty there instead.
ssh_run() { ssh ${SSH_OPTS[@]+"${SSH_OPTS[@]}"} "$@"; }

if [[ -n "${DEPLOY_HOST}" ]]; then
    log "deploying to ${DEPLOY_HOST} when the build finishes"
    # A connection now, so a wrong host or a missing key shows up before the
    # compile rather than after it. A warning and not an error, because a login
    # that legitimately wants a passphrase looks the same from here as a broken
    # one; BatchMode is for the probe only, since a probe that blocks on a prompt
    # at the start of an unattended build is worse than one that fails.
    ssh_run -o BatchMode=yes -o ConnectTimeout=10 "${DEPLOY_HOST}" true 2>/dev/null ||
        log "warning: ${DEPLOY_HOST} did not answer without a prompt; the deploy may ask for one"
fi

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

    log "building in docker (${platform}); the first build compiles ~1000 crates and V8's bindings"
    DOCKER_BUILDKIT=1 docker build \
        --platform "${platform}" \
        --file "${REPO_ROOT}/scripts/static-build.Dockerfile" \
        --target export \
        --output "type=local,dest=${STAGE}" \
        --build-arg "RUST_VERSION=${RUST_VERSION}" \
        --build-arg "NODE_VERSION=${NODE_VERSION}" \
        --build-arg "TARGET=${TARGET}" \
        --build-arg "PREFIX=${PREFIX}" \
        --build-arg "SC_BUILD_ADMIN=${BUILD_UI}" \
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
    (
        cd "${REPO_ROOT}"
        # `SC_BUNDLE_PREFIX` re-roots the bundle paths compiled into the binary at
        # the install prefix: the bundles are built here, but the binary is going
        # somewhere this checkout does not exist.
        SC_BUNDLE_PREFIX="${PREFIX}" \
        SC_BUILD_ADMIN="${BUILD_UI}" \
        RUSTFLAGS="-C target-feature=+crt-static${RUSTFLAGS:+ ${RUSTFLAGS}}" \
            cargo build --release --target "${TARGET}" -p sc-cli ${JOBS:+--jobs "${JOBS}"}
    )

    mkdir -p "${STAGE}/bin"
    cp "${REPO_ROOT}/target/${TARGET}/release/feldspar" "${STAGE}/bin/feldspar"
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
}

case "${MODE}" in
    docker) build_docker ;;
    native) build_native ;;
esac

BINARY="${STAGE}/bin/feldspar"
[[ -f "${BINARY}" ]] || { echo "error: the build produced no binary at ${BINARY}" >&2; exit 1; }
chmod +x "${BINARY}"

if [[ ${STRIP} -eq 1 ]]; then
    # A statically linked V8 carries a lot of symbol table. Stripping is what
    # takes the artifact from ~200 MB to something worth copying to a VM. Best
    # effort: a missing cross `strip` is not a reason to fail a good build.
    strip_tool="strip"
    [[ "${TARGET}" == aarch64-* && "$(uname -m)" != aarch64 ]] && strip_tool="aarch64-linux-gnu-strip"
    if command -v "${strip_tool}" >/dev/null; then
        log "stripping symbols with ${strip_tool}"
        "${strip_tool}" "${BINARY}"
    else
        log "warning: ${strip_tool} not found; shipping unstripped (--no-strip to silence)"
    fi
fi

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

# Static linkage is the property this whole script exists to produce, so it is
# checked rather than assumed — a stray `-C target-feature=-crt-static` in a
# `.cargo/config.toml`, or a dependency that insists on a shared library, would
# otherwise be discovered on the destination VM.
assert_static() {
    local described
    described="$(file -b "${BINARY}")"
    case "${described}" in
        *"static-pie linked"*|*"statically linked"*) ;;
        *)
            echo "error: the binary is not statically linked:" >&2
            echo "  ${described}" >&2
            command -v ldd >/dev/null && ldd "${BINARY}" >&2 || true
            exit 1
            ;;
    esac
    log "linkage: ${described%%,*} — $(du -h "${BINARY}" | cut -f1)"
}

# The claim in this script's header, executed: the binary starts and prints its
# usage on a glibc distribution and on a musl one. Skipped when the target is not
# this machine's architecture (nothing here would run it) or docker is absent.
smoke_test_distros() {
    command -v docker >/dev/null || { log "skipping distro smoke test (no docker)"; return 0; }
    local host_arch="$(uname -m)"
    case "${TARGET}" in
        x86_64-*)  [[ "${host_arch}" == "x86_64"  ]] || { log "skipping distro smoke test (foreign architecture)"; return 0; } ;;
        aarch64-*) [[ "${host_arch}" == "aarch64" ]] || { log "skipping distro smoke test (foreign architecture)"; return 0; } ;;
    esac

    # The binary prints its usage on stderr and exits 0, so the output is held
    # rather than streamed: it is a page of text per image, and it is only worth
    # reading when the run fails.
    local image output
    for image in debian:12-slim alpine:3.20; do
        log "smoke test on ${image}"
        if ! output="$(docker run --rm --network none \
                -v "${BINARY}:/usr/local/bin/feldspar:ro" \
                "${image}" /usr/local/bin/feldspar 2>&1)"; then
            echo "error: the binary does not run on ${image}:" >&2
            printf '%s\n' "${output}" >&2
            exit 1
        fi
    done
}

if [[ ${VERIFY} -eq 1 ]]; then
    assert_static
    smoke_test_distros
fi

# ---------------------------------------------------------------------------
# Package
# ---------------------------------------------------------------------------

# The host-side installer travels inside the artifact. A machine that has just
# downloaded a tarball has no checkout to fetch it from, and it is the second of
# the two commands a new host needs — `install.sh` puts the tree at ${PREFIX},
# `setup-host.sh` gives it a database, a service account and a unit. It is not
# put on PATH: it is run once.
readonly SETUP_HOST="${REPO_ROOT}/scripts/setup-host.sh"
[[ -f "${SETUP_HOST}" ]] ||
    { echo "error: ${SETUP_HOST} is missing; it belongs in the artifact" >&2; exit 1; }
cp "${SETUP_HOST}" "${STAGE}/setup-host.sh"
chmod +x "${STAGE}/setup-host.sh"

cat > "${STAGE}/install.sh" <<EOF
#!/usr/bin/env sh
# Install this artifact at the prefix it was built for.
#
# The prefix is not a choice at install time: the binary carries \`${PREFIX}/ui/...\`
# as the location of the admin UI and the IDE, compiled in. To install elsewhere,
# rebuild with \`scripts/build-static.sh --prefix <path>\`.
set -eu

PREFIX="${PREFIX}"
SRC="\$(cd -- "\$(dirname -- "\$0")" && pwd)"

if [ "\$(id -u)" -ne 0 ] && [ ! -w "\$(dirname "\${PREFIX}")" ]; then
    echo "install: \${PREFIX} needs root (re-run with sudo)" >&2
    exit 1
fi

# Contents, not directories: \`cp -r bin \${PREFIX}/\` would put the binary in
# \${PREFIX}/bin/bin the second time this is run, which is the upgrade path.
mkdir -p "\${PREFIX}/bin"
cp -r "\${SRC}/bin/." "\${PREFIX}/bin/"
if [ -d "\${SRC}/ui" ]; then
    mkdir -p "\${PREFIX}/ui"
    cp -r "\${SRC}/ui/." "\${PREFIX}/ui/"
fi
# The bundled modules. Copied over an existing tree rather than replacing it:
# what an admin installed lives in the modules directory, not here, so this is
# only ever the catalog this release ships.
if [ -d "\${SRC}/plugins" ]; then
    mkdir -p "\${PREFIX}/plugins"
    cp -r "\${SRC}/plugins/." "\${PREFIX}/plugins/"
fi
chmod +x "\${PREFIX}/bin/feldspar"

# Beside the tree rather than inside bin/: it is run once, by a person, and has
# no business on PATH. Run from there it knows it is looking at an installed
# artifact and does not offer to build one from source.
cp "\${SRC}/setup-host.sh" "\${PREFIX}/setup-host.sh"
chmod +x "\${PREFIX}/setup-host.sh"

echo "installed \${PREFIX}/bin/feldspar"
echo
echo "next, set this host up to run it — packages, PostgreSQL, a service account,"
echo "/etc/feldspar/feldspar.toml and a systemd unit:"
echo "  sudo \${PREFIX}/setup-host.sh --domain example.com"
echo "  sudo \${PREFIX}/setup-host.sh --help       # every option"
echo "  sudo \${PREFIX}/setup-host.sh --dry-run    # what it would do, doing nothing"
echo
echo "that also puts feldspar on PATH. On a host that is already set up, only the"
echo "service needs restarting:  sudo systemctl restart feldspar"
EOF
chmod +x "${STAGE}/install.sh"

cat > "${STAGE}/README" <<EOF
feldspar ${VERSION}${GIT_DESC:+ (${GIT_DESC})} — ${TARGET}

This binary is statically linked: it has no shared-library dependencies and no
interpreter, so it runs on glibc distributions (Debian, Ubuntu, RHEL) and on musl
ones (Alpine) without anything installed alongside it.

Install
  sudo ./install.sh
      copies this tree to ${PREFIX}
  sudo ${PREFIX}/setup-host.sh --domain example.com
      sets this host up to run it: packages, PostgreSQL, the feldspar service
      account, /etc/feldspar/feldspar.toml and a systemd unit. Once, on a new
      host; --dry-run first if you would rather read it than run it.
  ${PREFIX}/bin/feldspar
      prints the available commands

On a host that already has a database, a service account and a unit, the first
command is the whole upgrade — restart the service afterwards.

Where a newer one comes from
  ${R2_PUBLIC_BASE}/${R2_OBJECT}
  ${R2_PUBLIC_BASE}/${R2_OBJECT}.sha256
  That URL is always the latest build; it carries no version, so there is nothing
  to look up. \`sha256sum -c ${R2_OBJECT}.sha256\` checks a download.

Run
  feldspar serve --database-url postgres://user:pass@host/db
  feldspar serve --environment prod     # from feldspar.toml

  Database settings can come from flags, the environment, or a feldspar.toml
  read from /etc/feldspar/ or ~/.config/feldspar/.

What is in the tarball
  bin/feldspar          the server and management CLI
  setup-host.sh         the host setup, run once from ${PREFIX} after install.sh.
                        Debian and Ubuntu; --dry-run prints what it would do.
  plugins/              the modules this release ships with — an RSS table
                        provider and a Markdown renderer today. They are not
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

What the target machine still needs
  Nothing to run the server: this binary has no library dependencies, and none of
  the build-time toolchain (libclang, cmake, a C compiler) is needed here.
  A database — Postgres, or a SQLite file.
  npm and node, but only if applications are built or modules installed on this
  machine: \`feldspar build-app\`, the admin UI's Build button and a module install
  all run npm. A server that only serves an already-built application does not
  need them.

Limits of a static binary
  dlopen does not work, so native Node addons (.node, loaded through deno_napi)
  cannot be loaded by modules. Nothing in the server itself needs one.
  Hostnames are resolved by the binary's own resolver (/etc/resolv.conf and
  /etc/hosts), not by glibc's NSS — which in a static binary would dlopen a
  second libc and crash the process. /etc/nsswitch.conf does not affect this
  server: a name it must reach belongs in DNS or in /etc/hosts.
EOF

mkdir -p "${OUTPUT_DIR}"
readonly TARBALL="${OUTPUT_DIR}/${NAME}.tar.gz"

# Packaged with the release name as the top-level directory, so unpacking in a
# scratch directory does not scatter `bin/` and `ui/` over whatever is there.
tar -czf "${TARBALL}" -C "${STAGE}" --transform "s,^\.,${NAME}," .
(cd "${OUTPUT_DIR}" && sha256sum "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256")

log "wrote ${TARBALL} ($(du -h "${TARBALL}" | cut -f1))"

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

# Copy the artifact to the R2 bucket under a fixed, version-less name, so that an
# installer can always fetch the same URL and get the newest build.
publish_to_r2() {
    if [[ "${GIT_DESC}" == *-dirty ]]; then
        log "warning: publishing a build from a dirty checkout (${GIT_DESC})"
    fi

    # The checksum file written next to the tarball names the versioned file it
    # was computed over, which is not the name it has in the bucket — a
    # `sha256sum -c` after downloading would look for a file that is not there.
    # The published copy therefore names the published object.
    local published_sha="${STAGE}/${R2_OBJECT}.sha256"
    printf '%s  %s\n' \
        "$(cut -d' ' -f1 < "${TARBALL}.sha256")" "${R2_OBJECT}" > "${published_sha}"

    log "uploading $(du -h "${TARBALL}" | cut -f1) to r2://${R2_BUCKET}/${R2_OBJECT}"
    (cd "${REPO_ROOT}" && npx wrangler r2 object put \
        "${R2_BUCKET}/${R2_OBJECT}" -f "${TARBALL}" --remote)

    log "uploading r2://${R2_BUCKET}/${R2_OBJECT}.sha256"
    (cd "${REPO_ROOT}" && npx wrangler r2 object put \
        "${R2_BUCKET}/${R2_OBJECT}.sha256" -f "${published_sha}" --remote)

    log "published ${NAME}.tar.gz as ${R2_BUCKET}/${R2_OBJECT}"
    if [[ "${R2_BUCKET}" == "${DEFAULT_R2_BUCKET}" ]]; then
        cat <<EOF

It is now what a new host downloads:

  curl -fLO ${R2_PUBLIC_BASE}/${R2_OBJECT}
  curl -fLO ${R2_PUBLIC_BASE}/${R2_OBJECT}.sha256
  sha256sum -c ${R2_OBJECT}.sha256
  tar -xzf ${R2_OBJECT} && sudo feldspar-*/install.sh
  sudo ${PREFIX}/setup-host.sh --domain example.com
EOF
    fi
}

if [[ ${RELEASE} -eq 1 ]]; then
    publish_to_r2
fi

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

# The three commands this script prints when --deploy is *not* given, run for
# you: copy, unpack, install. The tarball goes over the ssh connection itself
# rather than through scp, because
# scp and ssh spell their options differently (-P against -p for a port) and one
# --ssh-opt has to mean the same thing in both places.
deploy_to_host() {
    local remote_tarball="${REMOTE_TMP}/${NAME}.tar.gz"
    local remote_script="${REMOTE_TMP}/${NAME}.deploy.sh"

    log "copying $(du -h "${TARBALL}" | cut -f1) to ${DEPLOY_HOST}:${remote_tarball}"
    ssh_run "${DEPLOY_HOST}" \
        "mkdir -p '${REMOTE_TMP}' && cat > '${remote_tarball}'" < "${TARBALL}"

    # The remote side is a *file* on the remote rather than a command line or a
    # script on stdin: the quoting is then decided here once, and — the reason it
    # is a file and not stdin — the remote's stdin stays free for sudo to read a
    # password from. It is `sh`, not bash: the destination is whatever the
    # artifact runs on, Alpine included.
    ssh_run "${DEPLOY_HOST}" "cat > '${remote_script}'" <<REMOTE
set -eu

tarball="${remote_tarball}"
tree="${REMOTE_TMP}/${NAME}"

# What an interrupted deploy of this same version left behind; unpacking over it
# would mix two trees.
rm -rf "\${tree}"
tar -xzf "\${tarball}" -C "${REMOTE_TMP}"

if [ "\$(id -u)" -eq 0 ]; then
    sudo=""
elif command -v sudo >/dev/null 2>&1; then
    sudo="sudo"
else
    echo "error: installing to ${PREFIX} needs root, and this login is neither root nor has sudo" >&2
    exit 1
fi

# A running server holds ${PREFIX}/bin/feldspar open, and the kernel refuses to
# write over the executable of a live process ("Text file busy"), so the unit has
# to be stopped before install.sh copies the binary in. This is as late as it can
# be — the tarball is already here and unpacked — so the downtime is the install
# and nothing more.
#
# Only a unit that is *running* is stopped and started again: on a machine with
# no systemd, no feldspar.service, or a service deliberately left down, the
# install happens exactly as it did before and nothing is started behind the
# operator's back.
restart_unit=""
if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet feldspar 2>/dev/null; then
    echo "stopping feldspar.service"
    if \${sudo} systemctl stop feldspar; then
        restart_unit="yes"
    else
        echo "warning: could not stop feldspar.service; installing anyway" >&2
    fi
fi

\${sudo} "\${tree}/install.sh"

if [ -n "\${restart_unit}" ]; then
    echo "starting feldspar.service"
    \${sudo} systemctl start feldspar
fi

# The installed binary, run where it now lives: it prints its usage and exits 0,
# which is the claim this whole script exists to make — one file that runs on
# this machine with nothing installed beside it.
"${PREFIX}/bin/feldspar" >/dev/null

rm -rf "\${tree}" "\${tarball}" "${remote_script}"
REMOTE

    log "unpacking and installing on ${DEPLOY_HOST}"
    # -t so that sudo can prompt on the terminal this was started from; asked for
    # only when there is one, or ssh warns about a pty it cannot allocate.
    local tty=()
    [[ -t 0 && -t 1 ]] && tty=(-t)
    ssh_run "${tty[@]+"${tty[@]}"}" "${DEPLOY_HOST}" "sh '${remote_script}'"

    log "installed ${PREFIX}/bin/feldspar on ${DEPLOY_HOST}"
}

if [[ -n "${DEPLOY_HOST}" ]]; then
    deploy_to_host
    cat <<EOF

Installed on ${DEPLOY_HOST}. If this is a host that has never run feldspar, give
it a database, a service account and a unit — once:

  ssh -t ${DEPLOY_HOST} 'sudo ${PREFIX}/setup-host.sh --domain example.com'

Otherwise it is already running the new binary, or run it by hand with:

  ssh ${DEPLOY_HOST} '${PREFIX}/bin/feldspar serve --environment prod'
EOF
else
    cat <<EOF

Install it on the target VM with:

  scp ${TARBALL} vm:/tmp/
  ssh vm 'tar -xzf /tmp/${NAME}.tar.gz -C /tmp && sudo /tmp/${NAME}/install.sh'
  ssh vm 'sudo ${PREFIX}/setup-host.sh --domain example.com'   # first time only

or have this script do it next time:

  scripts/build-static.sh --deploy vm
EOF
fi
