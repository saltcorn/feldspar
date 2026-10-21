#!/usr/bin/env bash
# A throwaway CA and a server certificate for 127.0.0.1 and localhost, so the
# spike verifies certificates (§5.1) without trusting anything real.
set -euo pipefail
dir="$1"
mkdir -p "$dir"
cd "$dir"
[ -f server.crt ] && exit 0
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=feldspar spike CA" \
  -keyout ca.key -out ca.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >ext.cnf
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile ext.cnf -out server.crt 2>/dev/null
chmod 600 ca.key server.key
