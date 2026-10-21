#!/usr/bin/env bash
# The M1.4 spike, end to end (TODO-imap.md): start a local Dovecot and the SMTP
# sink, run the harness (crates/sc-module/tests/imap_spike.rs) on feldspar's
# Deno worker, stop the servers. Throwaway, like everything in this directory.
#
# Needs: a Debian/Ubuntu host (apt-get download, dpkg -x), openssl, node and
# npm, cargo. No root and no Docker.
set -euo pipefail
spike="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$spike/../../.." && pwd)"

(cd "$spike/servers" && npm install --no-audit --no-fund --silent)
"$spike/servers/dovecot.sh" start
node "$spike/servers/smtp-sink.mjs" >"$spike/run/sink.out" 2>&1 &
sink=$!
trap 'kill $sink 2>/dev/null; "$spike/servers/dovecot.sh" stop' EXIT
sleep 1

cd "$repo"
IMAP_SPIKE_DIR="$spike" cargo test -p sc-module --features deno-host --test it -j1 \
  imap_spike -- --ignored --nocapture "$@"
