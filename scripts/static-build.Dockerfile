# syntax=docker/dockerfile:1.7
#
# The hermetic builder behind `scripts/build-static.sh --docker` (the default).
#
# It exists for one reason: the static binary this repository ships has to be
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
# Two variants come out of this one file, chosen by `VARIANT`:
#
#   static           `+crt-static` on Debian 12: one file, no shared libraries,
#                    runs on any glibc or musl distribution. No Python.
#   debian13-python  dynamically linked on Debian 13 (`DEBIAN_SUITE=trixie`),
#                    with `--features python`: it links `libpython3.13`, so it
#                    runs on Debian 13 with that package installed and nowhere
#                    else. A static binary cannot embed CPython at all (see
#                    `scripts/build-static.sh`), which is why it is a second
#                    artifact and not a flag on the first.

ARG RUST_VERSION=1.97
ARG DEBIAN_SUITE=bookworm
ARG VARIANT=static

FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS builder
ARG VARIANT

# The C-side build requirements, in the order the crates that need them appear
# in the dependency graph. `zlib1g-dev` is the one that is easy to miss: a
# `+crt-static` link needs `libz.a`, and the shared library alone is not enough.
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
    && rm -rf /var/lib/apt/lists/*

# The Python variant links the distribution's CPython (`python3-dev` is
# `libpython3.13-dev` on trixie). PyO3 builds against `abi3-py311`, so this only
# decides the soname the binary asks for at run time.
RUN if [ "${VARIANT}" = debian13-python ]; then \
        apt-get update && apt-get install -y --no-install-recommends python3-dev \
        && rm -rf /var/lib/apt/lists/*; \
    fi

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
ENV SC_BUNDLE_PREFIX=${PREFIX}

# `+crt-static` is what makes the static artifact one file that runs on both a
# glibc distribution and a musl one — see the header of `scripts/build-static.sh`
# for why that, and not a musl target, is the way there. The Python variant is
# linked dynamically instead, and built with the `python` feature.
#
# The two cache mounts make a second build of a changed tree minutes rather than
# an hour. `target` is `sharing=locked` because two concurrent builds writing one
# cargo target directory is a corrupted directory, not a race that resolves.
# `CARGO_TARGET_DIR` is a fixed path rather than `${PREFIX}/target` so the cache
# mount's target is a literal: build-arg expansion inside a `--mount` flag is not
# something to rely on, and a cache mount that silently does not apply is a
# forty-minute rebuild that looks like a cache miss. Each variant builds in its
# own subdirectory of it: they differ in RUSTFLAGS, features and base image, and
# sharing one target directory would rebuild everything on every switch — or
# worse, link objects compiled against one Debian's headers into the other.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eu; \
    export SC_BUILD_ADMIN="${SC_BUILD_ADMIN}"; \
    export SC_BUILD_TIME="${SC_BUILD_TIME}" SC_BUILD_COMMIT="${SC_BUILD_COMMIT}"; \
    export CARGO_TARGET_DIR="/build/target/${VARIANT}"; \
    case "${VARIANT}" in \
        static) \
            export RUSTFLAGS="-C target-feature=+crt-static${RUSTFLAGS:+ ${RUSTFLAGS}}"; \
            features="" ;; \
        debian13-python) \
            export PYO3_PYTHON=python3; \
            features="--features python" ;; \
        *) echo "unknown VARIANT ${VARIANT}" >&2; exit 1 ;; \
    esac; \
    cargo build --release --target "${TARGET}" -p sc-cli ${features} ${JOBS:+--jobs "${JOBS}"}; \
    mkdir -p /out/bin; \
    cp "${CARGO_TARGET_DIR}/${TARGET}/release/feldspar" /out/bin/feldspar

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
