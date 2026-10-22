#!/usr/bin/env bash
# A local Dovecot for the mail module's integration tests (TODO-imap.md M8.7), run as the
# current user with no root and no Docker: Debian's packages are downloaded and unpacked into
# run/dovecot (or copied from the M1.4 spike's, when it has them), and the daemon runs from
# there with a config of our own.
#
#   test/servers/dovecot.sh start|stop|restart|running|clean
#   test/servers/dovecot.sh deliver <user> <folder> <file.eml>
#
# IMAP on 127.0.0.1:11143 (STARTTLS required) and 127.0.0.1:11993 (TLS), with certificates from
# run/certs/ca.crt. Users `ingest` and `other` (ingest mode), `mirror` (mirror mode),
# `ann1`, `ann2`, `bob1`, `bob2` (two users' two accounts each) and `live` (live-matrix.test.mjs),
# all with one password that has non-ASCII characters (PASSWORD). Every mailbox the tests use
# is created on login.
set -euo pipefail

servers="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
run="$servers/run"
root="$run/dovecot"
conf="$run/dovecot.conf"
spike="$servers/../../spike/run/dovecot"
PASSWORD='Grüße-€-pässwort'
USERS=(ingest other mirror ann1 ann2 bob1 bob2 live)
export LD_LIBRARY_PATH="$root/usr/lib/dovecot:$root/usr/lib/x86_64-linux-gnu"

doveconf() { "$root/usr/bin/doveconf" -c "$conf" "$@"; }

unpack() {
  [ -x "$root/usr/sbin/dovecot" ] && return 0
  mkdir -p "$run"
  if [ -x "$spike/usr/sbin/dovecot" ]; then
    cp -a "$spike" "$root"
  else
    mkdir -p "$run/debs" "$root"
    (cd "$run/debs" && apt-get download dovecot-core dovecot-imapd libunwind8 libstemmer0d libexttextcat-2.0-0 >/dev/null)
    for deb in "$run/debs"/*.deb; do dpkg -x "$deb" "$root"; done
  fi
  # The TLS module is loaded from /usr/lib/dovecot/modules, compiled in with no setting to
  # change it. Patch the string to a relative path of the same length or less (NUL-padded), and
  # link that name to the unpacked modules in each directory a Dovecot process runs in.
  python3 -I - "$root/usr/lib/dovecot/libdovecot.so.0.0.0" <<'PY'
import sys
path = sys.argv[1]
data = open(path, "rb").read()
old = b"/usr/lib/dovecot/modules\0"
new = b"spike-dovecot-modules\0".ljust(len(old), b"\0")
if data.count(new) >= 1 and data.count(old) == 0:
    sys.exit(0)
assert data.count(old) == 1, "the module path is not in libdovecot exactly once"
open(path, "wb").write(data.replace(old, new))
PY
}

configure() {
  "$servers/certs.sh" "$run/certs"
  mkdir -p "$run/base/login" "$run/state"
  : >"$run/passwd"
  for user in "${USERS[@]}"; do
    mkdir -p "$run/mail/$user"
    printf '%s:{PLAIN}%s\n' "$user" "$PASSWORD" >>"$run/passwd"
  done
  for dir in "$run/base" "$run/base/login" "${USERS[@]/#/$run/mail/}"; do
    ln -sfn "$root/usr/lib/dovecot/modules" "$dir/spike-dovecot-modules"
  done
  local lib="$root/usr/lib/dovecot" me group
  me="$(id -un)"
  group="$(id -gn)"
  cat >"$conf" <<CONF
dovecot_config_version = 2.4.1
dovecot_storage_version = 2.4.1
base_dir = $run/base
state_dir = $run/state
log_path = $run/dovecot.log
default_internal_user = $me
default_internal_group = $group
default_login_user = $me
protocols = imap
listen = 127.0.0.1
mail_plugin_dir = $lib/modules
import_environment {
  LD_LIBRARY_PATH = $LD_LIBRARY_PATH
}
auth_mechanisms = plain login
auth_allow_cleartext = no
# The tests log in wrongly on purpose; do not make the next login wait.
auth_failure_delay = 0

ssl = required
ssl_server_cert_file = $run/certs/server.crt
ssl_server_key_file = $run/certs/server.key

mail_driver = maildir
mail_home = $run/mail/%{user}
mail_path = ~/Maildir

passdb passwd-file {
  passwd_file_path = $run/passwd
}
userdb static {
  fields {
    uid = $(id -u)
    gid = $(id -g)
    home = $run/mail/%{user}
  }
}

namespace inbox {
  inbox = yes
  mailbox Sent {
    special_use = \\Sent
    auto = create
  }
  mailbox Drafts {
    special_use = \\Drafts
    auto = create
  }
  mailbox Trash {
    special_use = \\Trash
    auto = create
  }
  mailbox Junk {
    special_use = \\Junk
    auto = create
  }
  mailbox Processed {
    auto = create
  }
  mailbox Errors {
    auto = create
  }
}

service imap-login {
  executable = $lib/imap-login
  chroot =
  inet_listener imap {
    port = 11143
  }
  inet_listener imaps {
    port = 11993
    ssl = yes
  }
}
service imap {
  executable = $lib/imap
}
service auth {
  executable = $lib/auth
}
service auth-worker {
  executable = $lib/auth
}
service config {
  executable = $lib/config
}
service log {
  executable = $lib/log
}
service anvil {
  executable = $lib/anvil
}
service stats {
  executable = $lib/stats
}
service imap-urlauth-login {
  executable = $lib/imap-urlauth-login
}
service imap-urlauth {
  executable = $lib/imap-urlauth
}
service imap-urlauth-worker {
  executable = $lib/imap-urlauth-worker
}
service imap-hibernate {
  executable = $lib/imap-hibernate
}
service dict {
  executable = $lib/dict
}
service dict-async {
  executable = $lib/dict
}
service dict-expire {
  executable = $lib/dict-expire
}
service indexer {
  executable = $lib/indexer
}
service indexer-worker {
  executable = $lib/indexer-worker
}
service health-check {
  executable = $lib/script -p $lib/health-check.sh
}
service dns-client {
  executable = $lib/dns-client
}
service doveadm {
  executable = $lib/doveadm-server
}
CONF
}

running() {
  (exec 3<>/dev/tcp/127.0.0.1/11993) 2>/dev/null
}

# Stopping ends the open IMAP sessions too, as a server going down would: the master alone
# leaves its `imap` processes serving the connections they have.
stop() {
  if [ -f "$run/base/master.pid" ]; then
    local master children
    master="$(cat "$run/base/master.pid")"
    children="$(pgrep -P "$master" || true)"
    kill "$master" $children 2>/dev/null || true
    rm -f "$run/base/master.pid"
    for _ in $(seq 50); do
      running || return 0
      sleep 0.1
    done
  fi
}

start() {
  stop
  unpack
  configure
  # Dovecot runs /usr/bin/doveconf to read its config, a path compiled in. `doveconf -F
  # <program>` does that step itself and execs <program> with the parsed config.
  doveconf -F "$root/usr/sbin/dovecot" -c "$conf"
  for _ in $(seq 50); do
    running && { echo "dovecot on 11143 and 11993"; return 0; }
    sleep 0.1
  done
  echo "dovecot did not start; see $run/dovecot.log" >&2
  tail -20 "$run/dovecot.log" >&2
  return 1
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  # A restart keeps the mail: only the daemon goes away and comes back.
  restart) stop && start ;;
  running) running ;;
  # Every user's mail removed, with the server restarted on empty mailboxes.
  clean)
    stop
    rm -rf "$run/mail"
    start
    ;;
  # Delivered from outside any IMAP session, as a mail server would: doveadm writes into the
  # maildir and Dovecot notifies the sessions that IDLE.
  deliver)
    doveconf -F "$root/usr/bin/doveadm" -c "$conf" save -u "$2" -m "$3" <"$4"
    ;;
  *)
    sed -n '2,12p' "$0"
    exit 2
    ;;
esac
