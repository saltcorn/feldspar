# syntax=docker/dockerfile:1.7
#
# The hermetic builder behind `scripts/build-static.sh --docker` (the default).
#
# It exists for one reason: the binaries this repository ships have to be
# built by a toolchain nobody has to assemble by hand. The workspace needs a
# `libclang` (bindgen, reached through `deno_runtime`), a `cmake` and a C
# compiler (`aws-lc-sys`, `rusqlite`'s bundled amalgamation), a `zlib` with a
# **static** archive beside the shared one (`libz-sys`, via `deno_node`) and a
# Node toolchain for the two UI bundles. On a developer's machine that is a list
# of things to have forgotten; here it is a layer.
#
# Everything is built **at the install prefix** (`/opt/feldspar` by default).
# `crates/sc-cli/build.rs` compiles the admin and IDE bundle paths into the
# binary, and `SC_BUNDLE_PREFIX` is what makes those paths describe the machine
# the artifact is going to rather than this container.
#
# The export stage carries no image, only files: the script drives this with
# `--target export --output type=local`, so the result is a directory on the
# host and no image is left behind.
#
# What comes out is the one release artifact, built on Debian 12 (bookworm,
# glibc 2.36): dynamically linked, with `--features python` against a bundled
# CPython — python-build-standalone, pinned by release and SHA-256 in
# `scripts/build-static.sh` and unpacked at `${PREFIX}/python` — so it runs on
# every glibc ≥ 2.36 distribution with nothing Python-related on the host. It
# also carries bookworm's glibc and `.glibc` copies of the two executables that
# use it, for musl hosts (Alpine). See `scripts/lib/python-bundle.sh` and
# docs/OPERATIONS.md §1. Published as `feldspar-linux-<arch>.tar.gz`.

ARG RUST_VERSION=1.97
ARG DEBIAN_SUITE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS builder

# The C-side build requirements, in the order the crates that need them appear
# in the dependency graph. `patchelf` makes the `.glibc` copies. The
# distribution's Python is only a build tool here: the binary links the bundled
# one.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        libclang-dev \
        cmake \
        ninja-build \
        python3 \
        pkg-config \
        zlib1g-dev \
        binutils \
        xz-utils \
        curl \
        ca-certificates \
        patchelf \
    && rm -rf /var/lib/apt/lists/*

# Node from nodejs.org rather than the distribution: the UI bundles are built by
# Vite 5, whose floor is Node 18, and a Debian stable's Node is whatever that
# release froze. Pinned by full version so a rebuild of an old tag gets the same
# toolchain it did the first time.
ARG NODE_VERSION=22.14.0
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
        amd64) node_arch=x64 ;; \
        arm64) node_arch=arm64 ;; \
        *) echo "unsupported build architecture $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
        | tar -xJ -C /usr/local --strip-components=1 --exclude CHANGELOG.md --exclude README.md --exclude LICENSE

ARG TARGET=x86_64-unknown-linux-gnu
RUN rustup target add "${TARGET}"

# The source lives at the prefix it will be installed to. Two things then line
# up for free: the bundle paths `build.rs` records, and any absolute path that
# ends up in a build artifact.
ARG PREFIX=/opt/feldspar

# The bundled CPython, at `${PREFIX}/python` — inside this container,
# so the interpreter PyO3 configures the build from, the library it links and
# the tree that ships are one directory at the path it is installed at. Before
# the source is copied, so a source change does not download it again. The pin
# is `scripts/build-static.sh`'s and arrives as build arguments; there is no
# default here, because a second copy of a SHA-256 is one that goes stale.
ARG PBS_URL=""
ARG PBS_SHA256=""
COPY scripts/lib/python-bundle.sh /usr/local/bin/feldspar-python-bundle
RUN set -eu; \
    [ -n "${PBS_URL}" ] || { echo "the build needs PBS_URL and PBS_SHA256 (scripts/build-static.sh passes them)" >&2; exit 1; }; \
    feldspar-python-bundle fetch "${PBS_URL}" "${PBS_SHA256}" "${PREFIX}" "${PREFIX}/python"

WORKDIR ${PREFIX}
COPY . ${PREFIX}

# `SC_BUILD_ADMIN` decides whether the UI bundles are built at all; when they
# are, `SC_BUNDLE_PREFIX` decides which path the binary carries for them.
ARG SC_BUILD_ADMIN=1
# The build's identity, from `scripts/build-static.sh` (the context has no `.git`
# to read it from): when the script ran, and the commit it built. Compiled into
# the binary for Settings → System; empty means "not a release build".
ARG SC_BUILD_TIME=""
ARG SC_BUILD_COMMIT=""
ARG JOBS=
# Stripped here rather than on the host, because the
# `.glibc` copy has to be made from the stripped binary.
ARG STRIP=1
ENV SC_BUNDLE_PREFIX=${PREFIX}

# Linked dynamically, with the `python` feature against the bundled CPython,
# finding its `libpython` through a RUNPATH of `$ORIGIN/../python/lib` — beside
# the binary wherever the tree is, never the host's. (RUSTFLAGS reach no shell:
# `$ORIGIN` arrives at the linker as written.)
#
# The two cache mounts make a second build of a changed tree minutes rather than
# an hour. `target` is `sharing=locked` because two concurrent builds writing one
# cargo target directory is a corrupted directory, not a race that resolves.
# `CARGO_TARGET_DIR` is a fixed path rather than `${PREFIX}/target` so the cache
# mount's target is a literal: build-arg expansion inside a `--mount` flag is not
# something to rely on, and a cache mount that silently does not apply is a
# forty-minute rebuild that looks like a cache miss. (Its `linux` subdirectory
# is where the build has always gone, so an existing cache still applies.)
#
# The UI bundles are built by sc-cli's build script into `ui/*/dist` of this
# source tree, which `.dockerignore` keeps out of the context, so every build
# starts without them. The cached build script runs again only when a UI's
# sources change, so the `package.json` files it watches are touched first:
# otherwise a second build that changes only Rust code has no bundles to copy.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eu; \
    export SC_BUILD_ADMIN="${SC_BUILD_ADMIN}"; \
    export SC_BUILD_TIME="${SC_BUILD_TIME}" SC_BUILD_COMMIT="${SC_BUILD_COMMIT}"; \
    export CARGO_TARGET_DIR="/build/target/linux"; \
    case "${SC_BUILD_ADMIN}" in 0|false|False|FALSE) ;; *) touch ui/*/package.json ;; esac; \
    export PYO3_PYTHON="${PREFIX}/python/bin/python3"; \
    export RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/../python/lib'"${RUSTFLAGS:+ ${RUSTFLAGS}}"; \
    cargo build --release --target "${TARGET}" -p sc-cli --features python ${JOBS:+--jobs "${JOBS}"}; \
    mkdir -p /out/bin; \
    cp "${CARGO_TARGET_DIR}/${TARGET}/release/feldspar" /out/bin/feldspar

# The runtime: the CPython it linked, then bookworm's glibc
# and the `.glibc` copies made from this image's libraries — this image, because
# they are the ones the binary was linked against.
RUN set -eu; \
    cp -a "${PREFIX}/python" /out/python; \
    case "${STRIP}" in 0|false|False|FALSE) strip=0 ;; *) strip=1 ;; esac; \
    feldspar-python-bundle glibc /out "${PREFIX}" "${strip}"

# Staged separately from the build so the cache mount above is not held open
# while the (large) copies happen.
#
# `plugins/` is the **bundled modules** — source only, with no dependency tree:
# npm and pip fetch what each one needs when an admin installs it, so this is a
# few kilobytes and not a vendored `node_modules`
# (`crates/sc-module/src/bundled.rs`). It is copied whatever `SC_BUILD_ADMIN`
# said, because there is nothing here to build.
#
# The bundles are every one `crates/sc-cli/build.rs` builds (its `BUNDLES`;
# `tests/build_script.rs` holds this list to that one). When the UI build is on
# each must be there: the binary carries its path under the prefix, and one left
# behind is a route answering "not built" on the installed server.
RUN set -eu; \
    case "${SC_BUILD_ADMIN}" in 0|false|False|FALSE) ui=0 ;; *) ui=1 ;; esac; \
    for bundle in admin ide saltcorn-ui builder analytics; do \
        if [ -d "ui/${bundle}/dist" ]; then \
            mkdir -p "/out/ui/${bundle}" && cp -r "ui/${bundle}/dist" "/out/ui/${bundle}/"; \
        elif [ "${ui}" = 1 ]; then \
            echo "ui/${bundle}/dist was not built" >&2; exit 1; \
        fi; \
    done; \
    if [ ! -d plugins ]; then echo "plugins/ is missing; it belongs in the artifact" >&2; exit 1; fi; \
    mkdir -p /out/plugins; \
    cp -r plugins/. /out/plugins/; \
    find /out/plugins -maxdepth 2 \
        \( -name node_modules -o -name __pycache__ -o -name '*.egg-info' -o -name dist -o -name build \) \
        -exec rm -rf {} + 2>/dev/null || true

# Files only: `--output type=local` writes this stage's filesystem to a host
# directory, so nothing is committed as an image.
FROM scratch AS export
COPY --from=builder /out/ /
