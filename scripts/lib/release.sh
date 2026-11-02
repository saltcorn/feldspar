# shellcheck shell=bash
#
# Publishing a release to the R2 bucket: the part of `--release` that does not
# depend on what was built or how (TODO-portable-release D15). Sourced by
# `scripts/build-static.sh`, and by the macOS build script once there is one
# (R5), so the two publish the same way: each tarball under its fixed,
# version-less names with a `.sha256` beside each, then the curl installer.
#
# The caller provides:
#   REPO_ROOT   the checkout; `npx wrangler` runs there, and scripts/install.sh
#               is published from it
#   R2_BUCKET   the bucket to upload to (default ${DEFAULT_R2_BUCKET}, set by
#               the caller after sourcing this, so `--bucket` can change it)
#   log         a function printing one progress line
#
# What the caller keeps: which names a tarball is published under (they carry
# its OS and architecture, D13), the checks on the build itself, and what it
# tells an operator to run after a download, which differs per platform.

# The bucket the published artifacts live in. The objects inside it are named
# without a version — `feldspar-linux-x86_64.tar.gz` — because they are the
# *latest* build: whatever fetches them wants the current one and has no version
# to ask for. They do carry the OS and architecture (TODO-portable-release D13),
# so that each build machine publishes only its own names.
readonly DEFAULT_R2_BUCKET="feldspar-latest-static"
# What that bucket is served as, and what the README tells an operator to
# download. Printed after a publish so that what was uploaded and what people
# fetch are visibly the same thing. A --bucket somewhere else has no such URL and
# is not given one.
readonly R2_PUBLIC_BASE="https://feldspar-latest-static.saltcorn.com"

# Before the build: an absent npx, or a missing installer to publish, is worth
# finding out now and not after the compile. An empty --bucket is a usage error.
release_preflight() {
    [[ -n "${R2_BUCKET}" ]] || { echo "error: --bucket must not be empty" >&2; exit 2; }
    command -v npx >/dev/null ||
        { echo "error: --release needs npx (node), which is not on PATH" >&2; exit 1; }
    [[ -f "${REPO_ROOT}/scripts/install.sh" ]] ||
        { echo "error: scripts/install.sh is missing; --release publishes it" >&2; exit 1; }
}

# release_checksum TARBALL: write TARBALL.sha256 beside it, naming the file by
# its own (versioned) name, so `sha256sum -c` works in the directory it is in.
release_checksum() {
    local dir name
    dir="$(dirname -- "$1")"
    name="$(basename -- "$1")"
    (cd "${dir}" && sha256sum "${name}" > "${name}.sha256")
}

# release_warn_if_dirty GIT_DESC: a release from a checkout with edits in it is
# allowed, and said out loud.
release_warn_if_dirty() {
    if [[ "$1" == *-dirty ]]; then
        log "warning: publishing a build from a dirty checkout ($1)"
    fi
}

# release_upload TARBALL WORKDIR OBJECT...: upload TARBALL (which has its
# release_checksum beside it) to the bucket under each OBJECT name, each with
# its own .sha256. WORKDIR is a scratch directory for those checksum files.
release_upload() {
    local tarball="$1" workdir="$2"
    shift 2
    local object published_sha
    for object in "$@"; do
        # The checksum file written next to the tarball names the versioned file
        # it was computed over, which is not the name it has in the bucket — a
        # `sha256sum -c` after downloading would look for a file that is not
        # there. Each published copy therefore names its published object.
        published_sha="${workdir}/${object}.sha256"
        printf '%s  %s\n' \
            "$(cut -d' ' -f1 < "${tarball}.sha256")" "${object}" > "${published_sha}"

        log "uploading $(du -h "${tarball}" | cut -f1) to r2://${R2_BUCKET}/${object}"
        (cd "${REPO_ROOT}" && npx wrangler r2 object put \
            "${R2_BUCKET}/${object}" -f "${tarball}" --remote)

        log "uploading r2://${R2_BUCKET}/${object}.sha256"
        (cd "${REPO_ROOT}" && npx wrangler r2 object put \
            "${R2_BUCKET}/${object}.sha256" -f "${published_sha}" --remote)

        log "published $(basename -- "${tarball}") as ${R2_BUCKET}/${object}"
    done
}

# release_is_public: is this the bucket R2_PUBLIC_BASE serves? Only then is
# there a download URL to print.
release_is_public() {
    [[ "${R2_BUCKET}" == "${DEFAULT_R2_BUCKET}" ]]
}

# The curl-to-sh installer, beside the artifacts it downloads. Uploaded after the
# tarballs rather than before: it fetches the version-less object, so for the
# moment between the two uploads a new installer would be fetching an old
# release, which is the less surprising of the two orders to land half-way.
release_upload_installer() {
    log "uploading r2://${R2_BUCKET}/install.sh"
    (cd "${REPO_ROOT}" && npx wrangler r2 object put \
        "${R2_BUCKET}/install.sh" -f "${REPO_ROOT}/scripts/install.sh" \
        --content-type "text/x-shellscript" --remote)
    if release_is_public; then
        cat <<EOF

The installer is published too; on a new host:

  curl -fsSL ${R2_PUBLIC_BASE}/install.sh | sh
EOF
    fi
}
