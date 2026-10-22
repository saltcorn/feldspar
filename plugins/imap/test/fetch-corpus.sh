#!/usr/bin/env bash
# Fetch the public mail corpus the parser tests run over (TODO-imap.md M1.1).
#
# Every source is shallow-cloned (or downloaded) into test/corpus/<source>/,
# which is git-ignored: the corpus is fetched, never committed. What was fetched
# is recorded in test/corpus.lock — a commit hash per git source, a sha256 per
# tarball — and a later run fetches exactly what the lock names, so two machines
# test against the same mails.
#
#   ./fetch-corpus.sh            fetch what corpus.lock names (or HEAD, for a
#                                source the lock does not have yet)
#   ./fetch-corpus.sh --update   fetch every source's HEAD and rewrite the lock
#   ./fetch-corpus.sh <source>…  only these sources
#
# Licences differ by source and are listed below. The tnef samples are GPL-2 and
# the SpamAssassin mails stay their senders' copyright: neither may be copied
# into test/fixtures/.
#
# The downloaded files are data. Nothing in them is run, sourced or built.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
corpus="$here/corpus"
lock="$here/corpus.lock"

# name | repository | licence | paths to keep (space separated)
GIT_SOURCES=(
  "stalwart|https://github.com/stalwartlabs/mail-parser.git|Apache-2.0 OR MIT|resources/eml/regressions resources/eml/malformed resources/eml/legacy resources/eml/thirdparty resources/eml/rfc resources/eml/issues"
  "zbateson|https://github.com/zbateson/mail-mime-parser.git|BSD-2-Clause|tests/_data/emails"
  "mikel-mail|https://github.com/mikel/mail.git|MIT|spec/fixtures/emails"
  "mime4j|https://github.com/apache/james-mime4j.git|Apache-2.0|core/src/test/resources/testmsgs core/src/test/resources/mimetools-testmsgs"
  "cpython|https://github.com/python/cpython.git|PSF-2.0|Lib/test/test_email/data"
  "php-mime-mail-parser|https://github.com/php-mime-mail-parser/php-mime-mail-parser.git|MIT|tests/mails"
  "flanker|https://github.com/mailgun/flanker.git|Apache-2.0|tests/fixtures/messages"
  "spamscope|https://github.com/SpamScope/mail-parser.git|Apache-2.0|tests/mails"
  "mailparser|https://github.com/nodemailer/mailparser.git|MIT|test/fixtures"
  "tnef|https://github.com/verdammelt/tnef.git|GPL-2.0|tests/files/datafiles"
)

# name | base url | licence | files
SPAMASSASSIN_URL="https://spamassassin.apache.org/old/publiccorpus"
SPAMASSASSIN_FILES="20030228_easy_ham.tar.bz2 20030228_easy_ham_2.tar.bz2 20030228_hard_ham.tar.bz2 20030228_spam.tar.bz2 20030228_spam_2.tar.bz2"

update=0
only=()
for arg in "$@"; do
  case "$arg" in
    --update) update=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) only+=("$arg") ;;
  esac
done

wanted() {
  [ ${#only[@]} -eq 0 ] && return 0
  local n
  for n in "${only[@]}"; do [ "$n" = "$1" ] && return 0; done
  return 1
}

# The lock's value for a key ("<source>" or "spamassassin/<file>"), or nothing.
locked() {
  [ -f "$lock" ] || return 0
  awk -v k="$1" '$1 == k { print $2 }' "$lock"
}

declare -A result

mkdir -p "$corpus"

for entry in "${GIT_SOURCES[@]}"; do
  IFS='|' read -r name repo licence paths <<<"$entry"
  wanted "$name" || continue
  dest="$corpus/$name"
  pin=""
  [ $update -eq 0 ] && pin="$(locked "$name")"
  echo "== $name ($licence)${pin:+ at $pin}"
  rm -rf "$dest"
  git -c advice.detachedHead=false init -q "$dest"
  git -C "$dest" remote add origin "$repo"
  git -C "$dest" sparse-checkout set --no-cone $(for p in $paths; do printf '/%s/ ' "$p"; done)
  git -C "$dest" fetch -q --depth 1 --filter=blob:none origin "${pin:-HEAD}"
  git -C "$dest" -c advice.detachedHead=false checkout -q FETCH_HEAD
  result[$name]="$(git -C "$dest" rev-parse HEAD)"
  for p in $paths; do
    if [ ! -d "$dest/$p" ]; then
      echo "   missing: $p (moved upstream?)" >&2
      exit 1
    fi
    printf '   %-45s %5d files\n' "$p" "$(find "$dest/$p" -type f | wc -l)"
  done
done

if wanted spamassassin; then
  dest="$corpus/spamassassin"
  echo "== spamassassin (copyright stays with the senders)"
  mkdir -p "$dest"
  for file in $SPAMASSASSIN_FILES; do
    tarball="$dest/$file"
    [ -f "$tarball" ] || curl -fsSL -o "$tarball" "$SPAMASSASSIN_URL/$file"
    sum="$(sha256sum "$tarball" | cut -d' ' -f1)"
    expected=""
    [ $update -eq 0 ] && expected="$(locked "spamassassin/$file")"
    if [ -n "$expected" ] && [ "$expected" != "$sum" ]; then
      echo "   $file: sha256 $sum, but corpus.lock says $expected" >&2
      exit 1
    fi
    result[spamassassin/$file]="$sum"
    tar -xjf "$tarball" -C "$dest"
  done
  # Each tarball holds a `cmds` file listing its messages; it is not a mail.
  find "$dest" -name cmds -type f -delete
  printf '   %-45s %5d files\n' "(all folders)" "$(find "$dest" -mindepth 2 -type f | wc -l)"
fi

# Rewrite the lock: the entries fetched now, the rest as they were.
{
  echo "# The mail corpus test/fetch-corpus.sh fetched last (TODO-imap.md M1.1)."
  echo "# <source> <commit, or sha256 of the tarball>"
  if [ -f "$lock" ]; then
    grep -v '^#' "$lock" | while read -r key value; do
      [ -z "$key" ] && continue
      [ -n "${result[$key]:-}" ] || echo "$key $value"
    done
  fi
  for key in "${!result[@]}"; do echo "$key ${result[$key]}"; done
} | { read -r a; read -r b; echo "$a"; echo "$b"; sort; } >"$lock.new"
mv "$lock.new" "$lock"
echo "corpus.lock written"
