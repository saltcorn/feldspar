# shellcheck shell=bash
#
# Publishing a release to the R2 bucket: the part of `--release` that does not
# depend on what was built or how (TODO-portable-release D15). Sourced by
# `scripts/build-static.sh`, and by the macOS build script once there is one
# (R5), so the two publish the same way.
#
# A release is a version, X.Y.Z. Publishing one is, in order:
#
#   release_version_preflight   before the build: is VERSION one that may be
#                               published, from this checkout, to this bucket?
#   release_prepare             still before the build: set the workspace
#                               version, commit it and tag the commit vX.Y.Z
#   (the caller builds and checks the tarball, then release_checksum)
#   release_push                commit the next version, X.Y.(Z+1)-dev, on
#                               the branch; push the branch and the tag
#   release_publish             the tarball under feldspar-X.Y.Z-<os>-<arch>;
#                               its version-less (latest) names, unless a
#                               newer version is already listed; X.Y.Z into
#                               versions.txt and latest.txt; and last the
#                               versioned name's .sha256, which marks this
#                               architecture of X.Y.Z as finished
#   release_upload_installer    the curl installer, when X.Y.Z is the latest
#
# Each architecture is built on a machine of its own, so one version is
# published by several runs. The first commits and tags, and leaves the
# branch at the next development version; the others run at that tag
# (`git checkout vX.Y.Z`) and publish only their own architecture.
#
# A run that failed half-way — a build, a push or an upload — is finished by
# running the same command again, from the tag or from the branch that has
# since moved on to X.Y.(Z+1)-dev: a version that is tagged, and whose
# versioned .sha256 for this architecture is not in the bucket yet, is
# *resumed* (release_version_preflight). Every bucket call is retried a few
# times first (release_retry), as R2 answers the odd 500 that a second try
# does not get. `scripts/revoke-release.sh` takes a published version back out.
#
# Between releases the workspace version is X.Y.Z-dev, a SemVer pre-release
# of the next patch release: what a build from a checkout calls itself, and
# older than X.Y.Z itself, which may be released from it (as may any newer
# X.Y.Z, for a minor or major release).
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

# The bucket the published artifacts live in. Each release is in it twice: under
# a name with its version — `feldspar-0.1.1-linux-x86_64.tar.gz`, never replaced
# once it is there — and under the version-less name of the *latest* release,
# `feldspar-linux-x86_64.tar.gz`, which each release overwrites: whatever
# fetches that wants the current one and has no version to ask for. Both carry
# the OS and architecture (TODO-portable-release D13), so that each build
# machine publishes only its own names.
readonly DEFAULT_R2_BUCKET="feldspar-latest-static"
# What that bucket is served as, and what the README tells an operator to
# download. Printed after a publish so that what was uploaded and what people
# fetch are visibly the same thing. A --bucket somewhere else has no such URL and
# is not given one.
readonly R2_PUBLIC_BASE="https://feldspar-latest-static.saltcorn.com"
# The name the first releases were published under, before names carried a
# platform. Scripts and hosts outside this repository still fetch it, so an
# x86_64 release is published under it too. It has always been an x86_64
# tarball, so only an x86_64 build publishes it.
readonly RELEASE_LEGACY_OBJECT="feldspar.tar.gz"
# Every platform a release is published for, as its names spell it: what
# scripts/revoke-release.sh looks for (the bucket cannot be listed with
# wrangler). A new platform's build script adds its name here.
# shellcheck disable=SC2034 # read by scripts/revoke-release.sh
readonly RELEASE_PLATFORMS=(linux-x86_64 linux-aarch64)

# release_versioned_object VERSION PLATFORM: the name that version's tarball
# for that platform keeps for good.
release_versioned_object() {
    echo "feldspar-$1-$2.tar.gz"
}

# release_latest_objects PLATFORM: the version-less names the latest release
# for that platform is published under, one per line; the first is the one a
# new host is pointed at.
release_latest_objects() {
    echo "feldspar-$1.tar.gz"
    [[ "$1" == linux-x86_64 ]] && echo "${RELEASE_LEGACY_OBJECT}"
    return 0
}

# How often a bucket call is tried, and how long to wait before the second try
# (then twice that, and so on). R2 now and then answers a request with a 500,
# "We encountered an internal error. Please try again."; 0.1.1's release died
# on one. Settable from the environment (the tests try once more, at once).
RELEASE_ATTEMPTS="${RELEASE_ATTEMPTS:-4}"
RELEASE_RETRY_DELAY="${RELEASE_RETRY_DELAY:-10}"
# What to tell the operator when the bucket still refuses after the retries:
# the caller sets it to the command that finishes the release.
RELEASE_RESUME_HINT="${RELEASE_RESUME_HINT:-}"

# Before the build: an absent npx, or a missing installer to publish, is worth
# finding out now and not after the compile. An empty --bucket is a usage error.
release_preflight() {
    [[ -n "${R2_BUCKET}" ]] || { echo "error: --bucket must not be empty" >&2; exit 2; }
    command -v npx >/dev/null ||
        { echo "error: --release needs npx (node), which is not on PATH" >&2; exit 1; }
    [[ -f "${REPO_ROOT}/scripts/install.sh" ]] ||
        { echo "error: scripts/install.sh is missing; --release publishes it" >&2; exit 1; }
    release_wrangler_ready
}

# release_wrangler ARGS...: `npx wrangler ARGS...` in the checkout. npx is told
# yes in advance: when wrangler is not installed it otherwise asks "Ok to
# proceed? (y)" before downloading it, and where wrangler's output is captured
# (release_r2_get) that question is never seen and the release waits on it.
release_wrangler() {
    (cd "${REPO_ROOT}" && npm_config_yes=true npx wrangler "$@")
}

# release_retry WHAT CMD...: run CMD, and again after a pause while it fails,
# RELEASE_ATTEMPTS times in all; then end the run, saying how to finish it.
release_retry() {
    local what="$1" attempt=1 delay
    shift
    until "$@"; do
        if [[ ${attempt} -ge ${RELEASE_ATTEMPTS} ]]; then
            echo "error: could not ${what}, after ${attempt} attempts" >&2
            [[ -n "${RELEASE_RESUME_HINT}" ]] && echo "${RELEASE_RESUME_HINT}" >&2
            exit 1
        fi
        delay=$((RELEASE_RETRY_DELAY * attempt))
        log "could not ${what} (attempt ${attempt} of ${RELEASE_ATTEMPTS}); trying again in ${delay}s"
        sleep "${delay}"
        attempt=$((attempt + 1))
    done
}

# release_put KEY FILE [WRANGLER ARGS...]: upload FILE to the bucket as KEY.
release_put() {
    local key="$1" file="$2"
    shift 2
    release_retry "upload r2://${R2_BUCKET}/${key}" \
        release_wrangler r2 object put "${R2_BUCKET}/${key}" -f "${file}" "$@" --remote
}

# release_r2_delete KEY: delete KEY from the bucket; one that is not there is
# already what was asked for.
release_r2_delete() {
    local key="$1"
    log "deleting r2://${R2_BUCKET}/${key}"
    release_retry "delete r2://${R2_BUCKET}/${key}" release_r2_delete_once "${key}"
}

release_r2_delete_once() {
    local out
    if out="$(release_wrangler r2 object delete "${R2_BUCKET}/$1" --remote 2>&1)"; then
        return 0
    fi
    [[ "${out}" == *"The specified key does not exist"* ]] && return 0
    printf '%s\n' "${out}" >&2
    return 1
}

# release_wrangler_ready: before anything reads the bucket, make sure wrangler
# runs, with its output in view. A wrangler npx has to download first is
# downloaded here, said out loud, and not silently in the middle of a read.
release_wrangler_ready() {
    local version
    log "checking for wrangler"
    if version="$(cd "${REPO_ROOT}" && npm_config_yes=false npx wrangler --version 2>/dev/null)"; then
        log "using wrangler ${version##*$'\n'}"
        return 0
    fi
    log "wrangler is not installed: npx is downloading it"
    release_wrangler --version ||
        release_die "could not run wrangler (npx wrangler --version); --release uploads with it"
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
    local object
    for object in "$@"; do
        release_upload_tarball "${tarball}" "${object}"
        release_upload_checksum "${tarball}" "${workdir}" "${object}"
        log "published $(basename -- "${tarball}") as ${R2_BUCKET}/${object}"
    done
}

# release_upload_tarball TARBALL OBJECT: the tarball alone, as OBJECT.
release_upload_tarball() {
    log "uploading $(du -h "$1" | cut -f1) to r2://${R2_BUCKET}/$2"
    release_put "$2" "$1"
}

# release_upload_checksum TARBALL WORKDIR OBJECT: OBJECT.sha256. The checksum
# file written next to the tarball names the versioned file it was computed
# over, which is not the name it has in the bucket — a `sha256sum -c` after
# downloading would look for a file that is not there. Each published copy
# therefore names its published object.
release_upload_checksum() {
    local tarball="$1" object="$3" published_sha="$2/$3.sha256"
    printf '%s  %s\n' \
        "$(cut -d' ' -f1 < "${tarball}.sha256")" "${object}" > "${published_sha}"
    log "uploading r2://${R2_BUCKET}/${object}.sha256"
    release_put "${object}.sha256" "${published_sha}"
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
# INSTALLER is the file to publish: the release's own (from a checkout of its
# tag), by default this checkout's.
release_upload_installer() {
    local installer="${1:-${REPO_ROOT}/scripts/install.sh}"
    log "uploading r2://${R2_BUCKET}/install.sh"
    release_put install.sh "${installer}" --content-type "text/x-shellscript"
    if release_is_public; then
        cat <<EOF

The installer is published too; on a new host:

  curl -fsSL ${R2_PUBLIC_BASE}/install.sh | sh
EOF
    fi
}

# ---------------------------------------------------------------------------
# Versions
# ---------------------------------------------------------------------------

# Every version published to the bucket, one per line, oldest first; and the
# newest of them, alone. What `install.sh --version` and a running server can
# ask "which releases are there?" and "is there a newer one?".
readonly RELEASE_VERSIONS_OBJECT="versions.txt"
readonly RELEASE_LATEST_OBJECT="latest.txt"

release_die() {
    echo "error: $*" >&2
    exit 1
}

# release_version_is_valid V: X.Y.Z in digits, no leading zeros. No
# pre-release or build suffix: their order is not what `sort -V` says.
release_version_is_valid() {
    [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
}

# release_version_ge A B: is version A the same as B or newer?
release_version_ge() {
    [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" == "$1" ]]
}

# release_version_base V: V without its pre-release suffix (0.1.2-dev -> 0.1.2).
release_version_base() {
    echo "${1%%-*}"
}

# release_next_dev X.Y.Z: the development version after it, X.Y.(Z+1)-dev.
release_next_dev() {
    local major minor patch
    IFS=. read -r major minor patch <<< "$1"
    echo "${major}.${minor}.$((patch + 1))-dev"
}

# release_newest: the newest valid version among the lines on stdin, or nothing.
release_newest() {
    { grep -E '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || true; } |
        sort -V | tail -n 1
}

# The workspace version: `[workspace.package] version`, which every crate
# inherits (`version.workspace = true`).
release_workspace_version() {
    release_cargo_version < "${1:-${REPO_ROOT}}/Cargo.toml"
}

# release_cargo_version: the workspace version in the Cargo.toml on stdin.
release_cargo_version() {
    sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' | head -n 1
}

# release_set_workspace_version OLD NEW: in Cargo.toml, and in Cargo.lock for
# the workspace's own packages — the entries with no `source`, which are all
# workspace members and all at OLD. Edited as text, not by `cargo update`, so
# that a build machine needs neither a host cargo nor the network for it; the
# build that follows finds the lock file already as it would have written it.
release_set_workspace_version() {
    local old="$1" new="$2" lock="${REPO_ROOT}/Cargo.lock"
    sed -i "/^\[workspace\.package\]/,/^\[/s/^version *= *\"[^\"]*\"/version = \"${new}\"/" \
        "${REPO_ROOT}/Cargo.toml"
    [[ "$(release_workspace_version)" == "${new}" ]] ||
        release_die "could not set the version in Cargo.toml"
    [[ -f "${lock}" ]] || return 0
    awk -v old="${old}" -v new="${new}" '
        function flush(   i, line) {
            for (i = 0; i < n; i++) {
                line = buf[i]
                if (!src && line == "version = \"" old "\"") line = "version = \"" new "\""
                print line
            }
            n = 0; src = 0
        }
        /^\[\[package\]\]/ { flush() }
        /^source = /       { src = 1 }
                           { buf[n++] = $0 }
        END                { flush() }
    ' "${lock}" > "${lock}.new"
    mv "${lock}.new" "${lock}"
}

# release_r2_get KEY FILE: download KEY from the bucket to FILE. Returns 0 when
# it is there and 1 when it is not. Any other failure — no login, no network —
# ends the run: "not there" is what lets a version be published, or the list
# of versions be started afresh, so it must not be guessed.
release_r2_get() {
    local key="$1" file="$2" out attempt=1
    log "reading r2://${R2_BUCKET}/${key}"
    while :; do
        rm -f "${file}"
        if out="$(release_wrangler r2 object get \
                "${R2_BUCKET}/${key}" --file "${file}" --remote 2>&1)"; then
            return 0
        fi
        [[ "${out}" == *"The specified key does not exist"* ]] && return 1
        printf '%s\n' "${out}" >&2
        [[ ${attempt} -lt ${RELEASE_ATTEMPTS} ]] ||
            release_die "could not read r2://${R2_BUCKET}/${key}, after ${attempt} attempts"
        log "could not read r2://${R2_BUCKET}/${key} (attempt ${attempt} of ${RELEASE_ATTEMPTS}); trying again in $((RELEASE_RETRY_DELAY * attempt))s"
        sleep "$((RELEASE_RETRY_DELAY * attempt))"
        attempt=$((attempt + 1))
    done
}

# release_published_versions WORKDIR: the bucket's versions.txt, on stdout
# (nothing before the first release).
release_published_versions() {
    local file="$1/${RELEASE_VERSIONS_OBJECT}"
    if release_r2_get "${RELEASE_VERSIONS_OBJECT}" "${file}"; then
        cat "${file}"
    fi
}

# release_version_preflight VERSION OBJECT WORKDIR: before the build, may
# VERSION be published from this checkout, with OBJECT (this architecture's
# versioned name) as its tarball? Sets
#   RELEASE_NEEDS_TAG  1: this run commits the version and tags it
#                      0: vVERSION is tagged already (another architecture's
#                         run, or one that did not finish)
#   RELEASE_ADVANCE    1: this run moves the branch on: the release commit and
#                         a commit of the next -dev version are pushed to it
#                         (a new release, or one whose push has not happened)
#                      0: only the tag is pushed
#   RELEASE_AT_TAG     0: HEAD is what is built (and tagged, if it is not yet)
#                      1: HEAD is elsewhere — the branch the release moved on
#                         to X.Y.(Z+1)-dev — and the release is resumed: the
#                         caller builds the tagged commit, not HEAD
#   RELEASE_REMOTE     the git remote the tag is pushed to
#   RELEASE_BRANCH     the branch pushed to, when RELEASE_ADVANCE is 1
#
# A new version — not tagged yet — may equal the version in Cargo.toml without
# its -dev suffix, or be newer, but not be older than it, than the newest
# vX.Y.Z tag, or than the newest version in versions.txt.
#
# A tagged version is what was decided when it was tagged, so none of that is
# asked again: it is published, or its publishing resumed, at its tag, unless
# this architecture of it is *finished*: its versioned .sha256 is in the
# bucket (uploaded last, release_publish) and the version is in versions.txt
# (which, before release_publish uploaded that checksum last, is what tells a
# finished release from one that stopped after its versioned tarball). A
# finished version is never replaced; scripts/revoke-release.sh takes one
# back out, and after that it may be published again.
release_version_preflight() {
    local version="$1" object="$2" workdir="$3"
    local tag="v${version}" current newest tagged_version
    release_version_is_valid "${version}" || {
        echo "error: --release takes the version to publish as X.Y.Z (e.g. 0.1.1), got '${version}'" >&2
        exit 2
    }
    git -C "${REPO_ROOT}" rev-parse --git-dir >/dev/null 2>&1 ||
        release_die "a release is tagged in git, and ${REPO_ROOT} is not a git checkout"
    [[ -z "$(git -C "${REPO_ROOT}" status --porcelain --untracked-files=no)" ]] ||
        release_die "the checkout has uncommitted changes; a release is built from a commit, so commit or stash them first"

    RELEASE_BRANCH="$(git -C "${REPO_ROOT}" symbolic-ref --quiet --short HEAD || true)"
    RELEASE_REMOTE="origin"
    if [[ -n "${RELEASE_BRANCH}" ]]; then
        RELEASE_REMOTE="$(git -C "${REPO_ROOT}" config "branch.${RELEASE_BRANCH}.remote" || echo origin)"
    fi
    # The remote's tags too: the other architectures' runs tag on other machines.
    log "fetching ${RELEASE_REMOTE}"
    git -C "${REPO_ROOT}" fetch --quiet --tags "${RELEASE_REMOTE}" ||
        release_die "could not fetch ${RELEASE_REMOTE}: the release commit and tag are checked against it and pushed to it"

    current="$(release_workspace_version)"
    [[ -n "${current}" ]] || release_die "could not read the version from Cargo.toml"
    RELEASE_AT_TAG=0
    RELEASE_ADVANCE=0

    if git -C "${REPO_ROOT}" rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
        if release_is_finished "${version}" "${object}" "${workdir}"; then
            release_die "r2://${R2_BUCKET}/${object} is already published, and a published version is never replaced; release a newer one (or take this one back out with scripts/revoke-release.sh ${version})"
        fi
        RELEASE_NEEDS_TAG=0
        if [[ "$(git -C "${REPO_ROOT}" rev-parse "${tag}^{commit}")" == "$(git -C "${REPO_ROOT}" rev-parse HEAD)" ]]; then
            [[ "${current}" == "${version}" ]] ||
                release_die "HEAD is tagged ${tag}, but Cargo.toml says ${current}"
            # On a branch whose upstream does not have the tagged commit yet: this
            # run's own release, made by a run that stopped before its push. It
            # moves the branch on as that run would have.
            if [[ -n "${RELEASE_BRANCH}" ]] &&
                git -C "${REPO_ROOT}" rev-parse -q --verify "@{upstream}" >/dev/null &&
                ! git -C "${REPO_ROOT}" merge-base --is-ancestor HEAD "@{upstream}"; then
                RELEASE_ADVANCE=1
            fi
        else
            tagged_version="$(git -C "${REPO_ROOT}" show "${tag}:Cargo.toml" | release_cargo_version)"
            [[ "${tagged_version}" == "${version}" ]] ||
                release_die "${tag} is tagged on a commit whose Cargo.toml says ${tagged_version:-no version}, not ${version}"
            RELEASE_AT_TAG=1
        fi
    else
        release_version_ge "${version}" "$(release_version_base "${current}")" ||
            release_die "${version} is older than the version in Cargo.toml, ${current}, and is not tagged ${tag}"
        newest="$(git -C "${REPO_ROOT}" tag -l 'v*' | sed 's/^v//' | release_newest)"
        if [[ -n "${newest}" ]] && ! release_version_ge "${version}" "${newest}"; then
            release_die "${version} is older than the newest release tag, v${newest}"
        fi
        newest="$(release_published_versions "${workdir}" | release_newest)"
        if [[ -n "${newest}" ]] && ! release_version_ge "${version}" "${newest}"; then
            release_die "${version} is older than the newest published release, ${newest} (r2://${R2_BUCKET}/${RELEASE_VERSIONS_OBJECT})"
        fi
        # Untagged but in the bucket: published from somewhere this remote
        # does not know about. Not ours to replace.
        if release_r2_get "${object}.sha256" "${workdir}/${object}.published.sha256"; then
            release_die "r2://${R2_BUCKET}/${object} is already published, and ${tag} is not tagged on ${RELEASE_REMOTE}; release a newer version"
        fi
        [[ -n "${RELEASE_BRANCH}" ]] ||
            release_die "a new release commits its version and tags it, and HEAD is detached; check out the branch to release from"
        RELEASE_NEEDS_TAG=1
        RELEASE_ADVANCE=1
    fi
    if [[ ${RELEASE_ADVANCE} -eq 1 ]]; then
        git -C "${REPO_ROOT}" rev-parse -q --verify "@{upstream}" >/dev/null ||
            release_die "${RELEASE_BRANCH} has no upstream branch to push the release commit to"
        git -C "${REPO_ROOT}" merge-base --is-ancestor "@{upstream}" HEAD ||
            release_die "${RELEASE_BRANCH} is behind $(git -C "${REPO_ROOT}" rev-parse --abbrev-ref "@{upstream}"); pull first, so that the release commit can be pushed"
    fi

    if [[ ${RELEASE_NEEDS_TAG} -eq 1 ]]; then
        log "releasing ${version} (Cargo.toml has ${current}): commit and tag ${tag}, build, push, publish"
    elif [[ ${RELEASE_AT_TAG} -eq 1 ]]; then
        log "resuming ${version}: ${tag} is tagged and not finished for this architecture; building and publishing ${tag}, leaving ${RELEASE_BRANCH:-HEAD} at ${current}"
    elif [[ ${RELEASE_ADVANCE} -eq 1 ]]; then
        log "${tag} is this commit and not pushed yet: finishing the release of ${version}"
    else
        log "${tag} is this commit: publishing this architecture of ${version}"
    fi
}

# release_is_finished VERSION OBJECT WORKDIR: is this architecture of VERSION
# published to the end? Its versioned .sha256 in the bucket, and VERSION in
# versions.txt.
release_is_finished() {
    local version="$1" object="$2" workdir="$3"
    local listed
    release_r2_get "${object}.sha256" "${workdir}/${object}.published.sha256" || return 1
    listed="$(release_published_versions "${workdir}")" || exit 1
    grep -qxF "${version}" <<< "${listed}"
}

# release_prepare VERSION: after release_version_preflight, before the build,
# so that the binary and the tarball carry VERSION and the tagged commit. When
# HEAD is not tagged yet: set the workspace version (if it differs), commit
# that, and tag the commit. Nothing is pushed until release_push, after the
# build; a build that fails leaves a local commit and tag, and the same
# command run again builds at them.
release_prepare() {
    local version="$1" current
    [[ ${RELEASE_NEEDS_TAG} -eq 1 ]] || return 0
    current="$(release_workspace_version)"
    if [[ "${current}" != "${version}" ]]; then
        log "setting the workspace version to ${version}"
        release_set_workspace_version "${current}" "${version}"
        git -C "${REPO_ROOT}" add Cargo.toml
        [[ -f "${REPO_ROOT}/Cargo.lock" ]] && git -C "${REPO_ROOT}" add Cargo.lock
        git -C "${REPO_ROOT}" commit --quiet -m "Release ${version}"
    fi
    git -C "${REPO_ROOT}" tag -a "v${version}" -m "feldspar ${version}"
    log "tagged v${version} (pushed once the build has passed). To abandon it instead:"
    log "  git tag -d v${version}$([[ "${current}" != "${version}" ]] && echo " && git reset --hard HEAD~1")"
}

# release_push VERSION: after the build. When the run moves the branch on,
# commit the next development version (X.Y.(Z+1)-dev) on top of the tagged
# release commit, then push the branch and the tag in one atomic push; else
# push the tag alone. Before any upload: a tag with no tarball yet is put
# right by running again, a published tarball whose commit is not on the
# remote is not. A failed push takes the -dev commit back off, leaving HEAD at
# the tag, which is where running the same command again picks up.
release_push() {
    local version="$1"
    if [[ ${RELEASE_ADVANCE} -eq 1 ]]; then
        local upstream next
        upstream="$(git -C "${REPO_ROOT}" rev-parse --symbolic-full-name "@{upstream}")"
        next="$(release_next_dev "${version}")"
        log "setting the workspace version to ${next}, for what comes after ${version}"
        release_set_workspace_version "${version}" "${next}"
        git -C "${REPO_ROOT}" add Cargo.toml
        [[ -f "${REPO_ROOT}/Cargo.lock" ]] && git -C "${REPO_ROOT}" add Cargo.lock
        git -C "${REPO_ROOT}" commit --quiet -m "Start ${next}"
        log "pushing ${RELEASE_BRANCH} and v${version} to ${RELEASE_REMOTE}"
        if ! git -C "${REPO_ROOT}" push --quiet --atomic "${RELEASE_REMOTE}" \
            "HEAD:${upstream#refs/remotes/"${RELEASE_REMOTE}"/}" "refs/tags/v${version}"; then
            git -C "${REPO_ROOT}" reset --quiet --hard "v${version}"
            release_die "could not push the release to ${RELEASE_REMOTE}; nothing was uploaded, and running the same command again retries"
        fi
    else
        log "pushing v${version} to ${RELEASE_REMOTE} (if it is not there yet)"
        git -C "${REPO_ROOT}" push --quiet "${RELEASE_REMOTE}" "refs/tags/v${version}" ||
            release_die "could not push v${version} to ${RELEASE_REMOTE}; nothing was uploaded"
    fi
}

# release_put_text KEY FILE: upload a small text file.
release_put_text() {
    log "uploading r2://${R2_BUCKET}/$1"
    release_put "$1" "$2" --content-type "text/plain; charset=utf-8"
}

# release_record_version VERSION WORKDIR: add VERSION to versions.txt (read
# again now, not at the preflight: another architecture's run may have
# written it since), and make latest.txt the newest. Called after the
# tarball's upload, so a version is listed only once it can be downloaded
# for at least one architecture.
release_record_version() {
    local version="$1" workdir="$2"
    local list="${workdir}/${RELEASE_VERSIONS_OBJECT}.new"
    local latest="${workdir}/${RELEASE_LATEST_OBJECT}"
    { release_published_versions "${workdir}"; echo "${version}"; } |
        { grep -E '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || true; } |
        sort -uV > "${list}"
    tail -n 1 "${list}" > "${latest}"
    release_put_text "${RELEASE_VERSIONS_OBJECT}" "${list}"
    release_put_text "${RELEASE_LATEST_OBJECT}" "${latest}"
    log "r2://${R2_BUCKET}/${RELEASE_VERSIONS_OBJECT} lists $(wc -l < "${list}") version(s), the newest $(cat "${latest}")"
}

# release_publish VERSION TARBALL WORKDIR VERSIONED LATEST...: after
# release_push, publish TARBALL (with its release_checksum beside it) as
# VERSION. The order is what lets a run that stops anywhere be finished by
# running it again (release_version_preflight):
#
#   1. the tarball as VERSIONED
#   2. as each LATEST name, with its .sha256 — unless versions.txt already has
#      a newer version, whose those names are (a resumed older release)
#   3. VERSION into versions.txt, and latest.txt
#   4. VERSIONED.sha256, last: this architecture of VERSION is finished. Until
#      it is there install.sh cannot install VERSIONED (it checks the
#      download against it), and a run again replaces what came before it.
#
# Sets RELEASE_IS_LATEST: 1 when VERSION is (now) the newest release, so the
# latest names and the installer are its; 0 when a newer one is.
release_publish() {
    local version="$1" tarball="$2" workdir="$3" versioned="$4"
    shift 4
    local newest
    newest="$(release_published_versions "${workdir}" | release_newest)"
    RELEASE_IS_LATEST=1
    if [[ -n "${newest}" ]] && ! release_version_ge "${version}" "${newest}"; then
        RELEASE_IS_LATEST=0
    fi
    release_upload_tarball "${tarball}" "${versioned}"
    if [[ ${RELEASE_IS_LATEST} -eq 1 ]]; then
        release_upload "${tarball}" "${workdir}" "$@"
    else
        log "${newest} is a newer release: leaving the latest names ($*) as they are"
    fi
    release_record_version "${version}" "${workdir}"
    release_upload_checksum "${tarball}" "${workdir}" "${versioned}"
    log "published $(basename -- "${tarball}") as ${R2_BUCKET}/${versioned}"
}
