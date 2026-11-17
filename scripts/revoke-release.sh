#!/usr/bin/env bash
#
# Take a published release back out of the R2 bucket: the version leaves
# versions.txt (and latest.txt), and its files go — every platform's
# `feldspar-X.Y.Z-<os>-<arch>.tar.gz` and `.sha256`.
#
# When the version is the newest, the version-less "latest" names
# (`feldspar-linux-<arch>.tar.gz`, and `feldspar.tar.gz` for x86_64) hold its
# tarball too. They are put back to the newest remaining version that has a
# tarball for that platform — downloaded from its versioned name, checked
# against its .sha256, uploaded again — or deleted when there is none. A
# version with a newer one listed is not what those names hold, so they are
# left alone.
#
# Git is not touched: vX.Y.Z stays tagged, and `scripts/build-static.sh
# --release X.Y.Z` publishes that tag again (scripts/lib/release.sh resumes a
# tagged version that is not in the bucket). To retire the version for good,
# delete the tag as well.
#
# Usage
# -----
#   scripts/revoke-release.sh 0.1.1                    # asks first
#   scripts/revoke-release.sh 0.1.1 --yes              # does not
#   scripts/revoke-release.sh 0.1.1 --bucket feldspar-staging
#   scripts/revoke-release.sh 0.1.1 --restore-latest   # put the latest names
#                                                      # back even without
#                                                      # evidence they hold it
#
# Needs npx on PATH and a wrangler login with access to the bucket, as
# --release does.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
# shellcheck source-path=SCRIPTDIR source=lib/release.sh
source "${REPO_ROOT}/scripts/lib/release.sh"

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

usage() {
    cat <<EOF
Usage: scripts/revoke-release.sh X.Y.Z [--yes] [--bucket NAME] [--restore-latest]

Remove release X.Y.Z from the R2 bucket: from versions.txt and latest.txt,
and every platform's feldspar-X.Y.Z-<os>-<arch>.tar.gz and .sha256. When it is
the newest release, the version-less latest names are put back to the newest
release that remains. The git tag vX.Y.Z is left as it is.

      --yes             Do not ask first.
      --bucket NAME     The bucket (default ${DEFAULT_R2_BUCKET}).
      --restore-latest  Put the latest names back to the newest remaining
                        release even when nothing says X.Y.Z is in them (a
                        release that stopped while uploading them).
  -h, --help            This message.
EOF
}

VERSION=""
YES=0
RESTORE_LATEST=0
R2_BUCKET="${DEFAULT_R2_BUCKET}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes|-y)         YES=1; shift ;;
        --restore-latest) RESTORE_LATEST=1; shift ;;
        --bucket)
            [[ $# -ge 2 ]] || { echo "error: --bucket needs a value" >&2; exit 2; }
            R2_BUCKET="$2"; shift 2 ;;
        -h|--help)        usage; exit 0 ;;
        -*)               echo "error: unknown option $1" >&2; usage >&2; exit 2 ;;
        *)
            [[ -z "${VERSION}" ]] || { echo "error: one version at a time" >&2; exit 2; }
            VERSION="$1"; shift ;;
    esac
done
[[ -n "${VERSION}" ]] || { usage >&2; exit 2; }
release_version_is_valid "${VERSION}" ||
    { echo "error: the version to revoke is X.Y.Z (e.g. 0.1.1), got '${VERSION}'" >&2; exit 2; }

release_preflight
WORK="$(mktemp -d "${TMPDIR:-/tmp}/feldspar-revoke.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

# What the bucket has of it.
LISTED_ALL="$(release_published_versions "${WORK}")"
LISTED=0
grep -qxF "${VERSION}" <<< "${LISTED_ALL}" && LISTED=1
REMAINING="$({ grep -vxF "${VERSION}" <<< "${LISTED_ALL}" || true; } |
    { grep -E '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || true; } | sort -uV)"
NEWEST_REMAINING="$(release_newest <<< "${REMAINING}")"
FOUND=()
for platform in "${RELEASE_PLATFORMS[@]}"; do
    object="$(release_versioned_object "${VERSION}" "${platform}")"
    if release_r2_get "${object}.sha256" "${WORK}/${object}.sha256"; then
        FOUND+=("${platform}")
    fi
done

# Do the latest names hold it? Only when nothing newer remains listed, and
# then for the platforms it has a tarball for (or all, when told to).
IS_NEWEST=0
if [[ -z "${NEWEST_REMAINING}" ]] || ! release_version_ge "${NEWEST_REMAINING}" "${VERSION}"; then
    IS_NEWEST=1
fi
RESTORE=()
if [[ ${IS_NEWEST} -eq 1 ]]; then
    if [[ ${RESTORE_LATEST} -eq 1 ]]; then
        RESTORE=("${RELEASE_PLATFORMS[@]}")
    elif [[ ${LISTED} -eq 1 || ${#FOUND[@]} -gt 0 ]]; then
        RESTORE=(${FOUND[@]+"${FOUND[@]}"})
    fi
fi

# previous_for PLATFORM: the newest remaining listed version with a tarball
# for PLATFORM, or nothing.
previous_for() {
    local platform="$1" candidate object
    while read -r candidate; do
        [[ -n "${candidate}" ]] || continue
        object="$(release_versioned_object "${candidate}" "${platform}")"
        # Its progress line to stderr: stdout is the answer.
        if release_r2_get "${object}.sha256" "${WORK}/${object}.sha256" >&2; then
            echo "${candidate}"
            return 0
        fi
    done < <(sort -rV <<< "${REMAINING}")
}

if [[ ${LISTED} -eq 0 && ${#FOUND[@]} -eq 0 && ${RESTORE_LATEST} -eq 0 ]]; then
    log "${VERSION} is not in r2://${R2_BUCKET}/${RELEASE_VERSIONS_OBJECT}, and no platform's .sha256 of it is there;"
    log "deleting its tarballs anyway, in case a release stopped after uploading one"
fi

PLAN=()
[[ ${LISTED} -eq 1 ]] && PLAN+=("remove ${VERSION} from ${RELEASE_VERSIONS_OBJECT}; ${RELEASE_LATEST_OBJECT} becomes ${NEWEST_REMAINING:-(none: it is deleted)}")
declare -A PREVIOUS=()
for platform in ${RESTORE[@]+"${RESTORE[@]}"}; do
    PREVIOUS[${platform}]="$(previous_for "${platform}")" || exit 1
    mapfile -t latest < <(release_latest_objects "${platform}")
    if [[ -n "${PREVIOUS[${platform}]}" ]]; then
        PLAN+=("put ${latest[*]} back to ${PREVIOUS[${platform}]}")
    else
        PLAN+=("delete ${latest[*]} (no other ${platform} release to put there)")
    fi
done
for platform in "${RELEASE_PLATFORMS[@]}"; do
    PLAN+=("delete $(release_versioned_object "${VERSION}" "${platform}") and its .sha256")
done

echo
echo "Revoking ${VERSION} from r2://${R2_BUCKET}:"
printf '  - %s\n' "${PLAN[@]}"
echo
if [[ ${YES} -eq 0 ]]; then
    [[ -t 0 ]] || { echo "error: not a terminal to ask on; pass --yes to revoke without asking" >&2; exit 1; }
    read -r -p "Type ${VERSION} to revoke it: " answer
    [[ "${answer}" == "${VERSION}" ]] || { echo "not revoked" >&2; exit 1; }
fi

# 1. Out of the list first, so nothing is pointed at it from here on.
if [[ ${LISTED} -eq 1 ]]; then
    printf '%s' "${REMAINING}${REMAINING:+$'\n'}" > "${WORK}/${RELEASE_VERSIONS_OBJECT}.new"
    release_put_text "${RELEASE_VERSIONS_OBJECT}" "${WORK}/${RELEASE_VERSIONS_OBJECT}.new"
    if [[ -n "${NEWEST_REMAINING}" ]]; then
        echo "${NEWEST_REMAINING}" > "${WORK}/${RELEASE_LATEST_OBJECT}"
        release_put_text "${RELEASE_LATEST_OBJECT}" "${WORK}/${RELEASE_LATEST_OBJECT}"
    else
        release_r2_delete "${RELEASE_LATEST_OBJECT}"
    fi
fi

# 2. The latest names, back to what came before it.
for platform in ${RESTORE[@]+"${RESTORE[@]}"}; do
    mapfile -t latest < <(release_latest_objects "${platform}")
    previous="${PREVIOUS[${platform}]}"
    if [[ -n "${previous}" ]]; then
        object="$(release_versioned_object "${previous}" "${platform}")"
        log "downloading r2://${R2_BUCKET}/${object}, to put it back under ${latest[*]}"
        release_r2_get "${object}" "${WORK}/${object}" ||
            release_die "r2://${R2_BUCKET}/${object}.sha256 is there and ${object} is not; nothing was put under ${latest[*]}"
        (cd "${WORK}" && sha256sum -c --status "${object}.sha256") ||
            release_die "r2://${R2_BUCKET}/${object} does not match its .sha256; nothing was put under ${latest[*]}"
        release_upload "${WORK}/${object}" "${WORK}" "${latest[@]}"
    else
        for object in "${latest[@]}"; do
            release_r2_delete "${object}.sha256"
            release_r2_delete "${object}"
        done
    fi
done

# 3. Its own files: the .sha256 first, so a run that stops half-way leaves no
# checksum naming a tarball that is gone.
for platform in "${RELEASE_PLATFORMS[@]}"; do
    object="$(release_versioned_object "${VERSION}" "${platform}")"
    release_r2_delete "${object}.sha256"
    release_r2_delete "${object}"
done

cat <<EOF

Revoked ${VERSION} from r2://${R2_BUCKET}. Running this again is safe, and
finishes a revoke that stopped half-way.

The git tag v${VERSION} is still there. \`scripts/build-static.sh --release ${VERSION}\`
would publish it again; to retire the version for good, delete the tag too:

  git tag -d v${VERSION} && git push origin :refs/tags/v${VERSION}
EOF
