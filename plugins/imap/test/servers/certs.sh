#!/usr/bin/env bash
# A throwaway CA and a server certificate for 127.0.0.1 and localhost, so the
# tests verify certificates (§5.1) without trusting anything real. Beside them,
# for the certificate settings: name.crt, from the same CA but for mail.test
# only (a server dialled by IP), and self.crt, self-signed for 127.0.0.1.
set -euo pipefail
dir="$1"
mkdir -p "$dir"
cd "$dir"
# Each set only once: a running test server may hold the earlier ones.
if [ ! -f server.crt ]; then
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=feldspar imap test CA" \
  -keyout ca.key -out ca.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >ext.cnf
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile ext.cnf -out server.crt 2>/dev/null
chmod 600 ca.key server.key
fi
[ -f self.crt ] && exit 0
openssl req -newkey rsa:2048 -nodes -subj "/CN=mail.test" -keyout name.key -out name.csr 2>/dev/null
printf 'subjectAltName=DNS:mail.test\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >name.cnf
openssl x509 -req -in name.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile name.cnf -out name.crt 2>/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=127.0.0.1" \
  -addext "subjectAltName=IP:127.0.0.1" -keyout self.key -out self.crt 2>/dev/null
chmod 600 name.key self.key
