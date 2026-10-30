# syntax=docker/dockerfile:1.7
#
# The production image: Saltcorn Feldspar on Docker's hardened Debian 13 base.
# An alternative to the systemd install of README §2, not a replacement for it.
#
#   docker login dhi.io                  # once: the base image is behind a free
#                                        # Docker account
#   docker build -t feldspar deploy/docker
#   docker compose -f deploy/docker/compose.yml up -d
#
# README §2.9 is the operator's guide; this header is the why.
#
# What goes in
# ------------
# The **released** `debian13-python` artifact from the bucket, not a build from
# source. That artifact is already the dynamically linked Debian 13 build with
# Python (scripts/build-static.sh), which is exactly this base, and building it
# again here would be the forty-minute, many-gigabyte V8 link that
# scripts/static-build.Dockerfile already does, kept in step with it by hand.
# The object has no version in its name yet, so a rebuild picks up the latest
# release only when the download is not cached:
#
#   docker build --pull --no-cache-filter fetch -t feldspar deploy/docker
#
# The base: `dhi.io/debian-base`, Docker Hardened Images' Debian 13. Its `-dev`
# variant, because the server needs things at run time that only a package
# manager installs sanely — Chromium for `view_app`, Node and npm (module
# installs and application builds shell out to npm), git (file stores), and
# CPython (the binary links libpython3.13). The shell-less runtime variant would
# mean copying Chromium's few hundred shared libraries across by hand. What the
# hardened base still buys is a signed base with SBOM and provenance, and
# Docker's hardened apt repository, which apt prefers over Debian's.
#
# How it runs
# -----------
# - As `feldspar` (uid 10001), never root. /opt/feldspar is root's and
#   read-only to it — the server runs user-authored JavaScript and Python, so the
#   account must be assumed reachable, and an account that cannot write the code
#   it executes cannot survive a restart. The systemd unit's rule, here too.
# - One writable path: the /var/lib/feldspar volume, which is HOME and
#   SC_DATA_DIR — file stores, npm's cache, the secrets key. Everything else can
#   be read-only (`--read-only --tmpfs /tmp`).
# - Plain HTTP on 8080, with TLS the business of a proxy in front:
#   FELDSPAR_SSL_MODE=proxy pins `ssl_mode`, so the public URLs are https://,
#   the session cookie is Secure, no ACME traffic happens, and the admin UI has
#   no SSL / TLS tab.
# - The database from the environment: DATABASE_URL, or PGHOST / PGUSER /
#   PGPASSWORD / PGDATABASE. No `--environment` on the command line, because
#   naming one makes a mounted feldspar.toml outrank those variables.
# - `tini` as PID 1, so the Chromium and npm processes the server starts are
#   reaped, and a stop signal reaches the server.
# - The entrypoint is `feldspar serve`, so arguments after the image name are
#   serve flags: `docker run … feldspar --base-domain example.com`.
#
# Chromium keeps its sandbox, which Docker's default seccomp profile does not
# allow; `feldspar seccomp-profile` prints the one that does (compose.yml uses
# it). Without it the server starts, says so, and has no `view_app`.

# ---------------------------------------------------------------------------
# The artifact: downloaded, checked, unpacked at the prefix it was built for.
# ---------------------------------------------------------------------------
FROM dhi.io/debian-base:trixie-debian13-dev AS fetch

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# The bucket scripts/build-static.sh --release publishes to, the object its
# Python variant is published as, and the prefix `crates/sc-cli/build.rs`
# compiled the UI bundle paths into the binary with (`SC_BUNDLE_PREFIX`): the
# tree has to be unpacked exactly there.
WORKDIR /fetch
RUN set -eu; \
    RELEASE_URL=https://feldspar-latest-static.saltcorn.com; \
    RELEASE_OBJECT=feldspar-debian13-python.tar.gz; \
    PREFIX=/opt/feldspar; \
    curl -fsSLO "${RELEASE_URL}/${RELEASE_OBJECT}"; \
    curl -fsSLO "${RELEASE_URL}/${RELEASE_OBJECT}.sha256"; \
    sha256sum -c "${RELEASE_OBJECT}.sha256"; \
    mkdir -p "${PREFIX}"; \
    tar -xzf "${RELEASE_OBJECT}" -C "${PREFIX}" --strip-components=1 --no-same-owner; \
    test "$(cat "${PREFIX}/VARIANT")" = debian13-python; \
    # The host installer and host setup are the systemd install's; nothing in
    # a container runs them.
    rm -f "${PREFIX}/install.sh" "${PREFIX}/setup-host.sh"; \
    chmod -R go-w "${PREFIX}"

# ---------------------------------------------------------------------------
# The image.
# ---------------------------------------------------------------------------
FROM dhi.io/debian-base:trixie-debian13-dev

# Node from NodeSource, at the major version scripts/setup-host.sh installs
# (NODE_MAJOR). Not Debian's nodejs: Debian 13 packages npm 9.2.0, which cannot
# install a module (`sc-module` needs 9.3.0 or newer).
RUN set -eu; \
    NODE_MAJOR=26; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl; \
    install -d -m 0755 /etc/apt/keyrings; \
    curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
        -o /etc/apt/keyrings/nodesource.asc; \
    chmod 0644 /etc/apt/keyrings/nodesource.asc; \
    printf 'deb [signed-by=/etc/apt/keyrings/nodesource.asc] https://deb.nodesource.com/node_%s.x nodistro main\n' \
        "${NODE_MAJOR}" > /etc/apt/sources.list.d/nodesource.list; \
    printf 'Package: nodejs\nPin: origin deb.nodesource.com\nPin-Priority: 600\n' \
        > /etc/apt/preferences.d/nodesource; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        nodejs \
        chromium \
        fonts-liberation \
        git \
        libpython3.13 \
        python3 \
        python3-venv \
        tini; \
    rm -rf /var/lib/apt/lists/*; \
    # The account, written out rather than made with useradd: the hardened
    # base ships neither useradd (Debian's `passwd`) nor nologin, and one line
    # in each file is all an account is. No password, no login shell.
    echo 'feldspar:x:10001:10001:Saltcorn Feldspar:/var/lib/feldspar:/usr/bin/false' >> /etc/passwd; \
    echo 'feldspar:x:10001:' >> /etc/group; \
    install -d -m 0700 -o 10001 -g 10001 /var/lib/feldspar

COPY --from=fetch /opt/feldspar /opt/feldspar

ENV PATH=/opt/feldspar/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    HOME=/var/lib/feldspar \
    SC_DATA_DIR=/var/lib/feldspar \
    FELDSPAR_SSL_MODE=proxy

VOLUME /var/lib/feldspar
WORKDIR /var/lib/feldspar
USER 10001:10001
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["tini", "--", "feldspar", "serve", "--bind", "0.0.0.0:8080"]
