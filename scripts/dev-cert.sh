#!/bin/sh
# Generates a self-signed TLS certificate for development in ./certs.
#
#   scripts/dev-cert.sh [domain]       # default: localhost
#
# XMPP requires TLS (STARTTLS) before the password; the client validates the
# certificate against the JID's domain, which is why the domain goes into the
# subjectAltName. CA:FALSE because `req -x509` marks the certificate as a CA by
# default, and rustls (rightly) rejects a CA certificate presented as a server
# certificate. In production, use a certificate from a CA (e.g. Let's Encrypt)
# for the domain.
set -eu
domain="${1:-localhost}"
cd "$(dirname "$0")/.."
dir=certs
mkdir -p "$dir"
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -subj "/CN=$domain" -addext "subjectAltName=DNS:$domain" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
# Development only: the container runs as another uid and needs to read the key.
chmod 644 "$dir/key.pem"
echo "certificado para '$domain' em $dir (use XMPP_CA=$dir/cert.pem no cliente)"
