#!/bin/bash
#
# Issue a Cockpit certificate for a host and make it trusted.
#
# Why this exists: a freshly installed Fedora host serves Cockpit with an
# auto-generated certificate issued for the *short* hostname only. That fails
# the name check for the two URLs an operator actually types - the FQDN and the
# bare address - so the browser refuses the page. This replaces it with a
# certificate carrying every name the host is reached by, signed by a private CA
# that is then trusted on the client.
#
# Usage, on the host whose Cockpit is being served (as root):
#
#   ./lab/cockpit-cert.sh issue  --host laboratory.lan --ip 10.10.1.129
#   ./lab/cockpit-cert.sh show
#
# Then take the printed CA file to each client and trust it:
#
#   # Fedora/RHEL client, for curl and Chrome-family browsers using the system store
#   sudo cp ca.crt /etc/pki/ca-trust/source/anchors/<name>.crt && sudo update-ca-trust
#
#   # Chromium/Edge, when it uses its own NSS database instead
#   certutil -A -n "<CA nickname>" -t "C,," -i ca.crt -d sql:$HOME/.pki/nssdb
#
# The script refuses to overwrite an existing CA key: reissuing a server
# certificate is safe, but silently replacing the CA would invalidate every
# certificate already signed by it.
set -euo pipefail

HOST=""
IP=""
ORG="Laboratory LAN"
OU="Infrastructure"
CA_CN="Laboratory LAN Cockpit CA 2026"
WORK=/root/cockpit-pki
CERTD=/etc/cockpit/ws-certs.d
DAYS_CA=3650
DAYS_CERT=825

usage() {
	sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'
	exit 2
}

while [ $# -gt 0 ]; do
	case "$1" in
	issue) shift ;;
	--host) HOST="${2:-}"; shift 2 ;;
	--ip) IP="${2:-}"; shift 2 ;;
	--org) ORG="${2:-}"; shift 2 ;;
	--ca-cn) CA_CN="${2:-}"; shift 2 ;;
	--work) WORK="${2:-}"; shift 2 ;;
	show)
		echo "=== deployed certificates ==="
		ls -laZ "$CERTD"/ 2>/dev/null
		for f in "$CERTD"/*.crt; do
			[ -e "$f" ] || continue
			echo
			echo "--- $f"
			openssl x509 -in "$f" -noout -subject -issuer -dates -ext subjectAltName
		done
		echo
		echo "=== what the listener presents ==="
		echo | openssl s_client -connect 127.0.0.1:9090 -servername "${HOST:-localhost}" 2>/dev/null \
			| openssl x509 -noout -subject -issuer -ext subjectAltName
		echo
		echo "=== CA file clients must trust ==="
		[ -f "$WORK/ca.crt" ] && openssl x509 -in "$WORK/ca.crt" -noout -subject -dates
		exit 0
		;;
	-h|--help) usage ;;
	*) echo "unknown argument: $1" >&2; usage ;;
	esac
done

[ -n "$HOST" ] || { echo "--host is required" >&2; usage; }
[ "$(id -u)" = "0" ] || { echo "run as root (writes $CERTD)" >&2; exit 1; }

# A bare address in the SAN is what makes https://<ip>/ work; without it the
# browser matches only the names.
SAN="DNS:${HOST},DNS:localhost,IP:127.0.0.1"
if [ -n "$IP" ]; then
	SAN="DNS:${HOST},DNS:localhost,IP:${IP},IP:127.0.0.1"
fi

mkdir -p "$WORK" "$CERTD"
cd "$WORK"

# ---------------------------------------------------------------------- CA
if [ -f ca.key ]; then
	echo "=== reusing the existing CA ($WORK/ca.key) ==="
else
	echo "=== creating the private CA ==="
	openssl genrsa -out ca.key 4096 2>/dev/null
	chmod 600 ca.key
	openssl req -x509 -new -key ca.key -sha256 -days "$DAYS_CA" -out ca.crt \
		-subj "/C=CN/O=${ORG}/OU=${OU}/CN=${CA_CN}" \
		-addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
		-addext "keyUsage=critical,keyCertSign,cRLSign"
fi

# ------------------------------------------------------------ server cert
echo "=== issuing a certificate for ${HOST}${IP:+ and $IP} ==="
openssl genrsa -out "$HOST.key" 2048 2>/dev/null
chmod 600 "$HOST.key"
openssl req -new -key "$HOST.key" -out "$HOST.csr" -subj "/C=CN/O=${ORG}/CN=${HOST}"

cat > san.cnf <<EOF
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=${SAN}
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid,issuer
EOF

openssl x509 -req -in "$HOST.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
	-out "$HOST.crt" -days "$DAYS_CERT" -sha256 -extfile san.cnf 2>/dev/null
openssl verify -CAfile ca.crt "$HOST.crt"
openssl x509 -in "$HOST.crt" -noout -subject -issuer -dates -ext subjectAltName

# ---------------------------------------------------------------- install
echo "=== installing for Cockpit ==="
# The auto-generated pair is removed rather than left beside ours: it is issued
# for the short name and would be a silent fallback nobody trusts.
rm -f "$CERTD"/0-self-signed.cert "$CERTD"/0-self-signed.key "$CERTD"/0-self-signed-ca.pem
install -m 0644 -o root -g root "$HOST.crt" "$CERTD/100-${HOST}.crt"
install -m 0600 -o root -g root "$HOST.key" "$CERTD/100-${HOST}.key"
# SELinux is enforcing on Fedora and Cockpit reads these as cert_t. Inheriting
# the label from the system's own pair is more reliable than guessing it.
chcon --reference=/etc/pki/tls/certs/localhost.crt "$CERTD/100-${HOST}.crt" 2>/dev/null || true
chcon --reference=/etc/pki/tls/private/localhost.key "$CERTD/100-${HOST}.key" 2>/dev/null || true

systemctl restart cockpit.socket
sleep 2
systemctl is-active cockpit.socket

echo
echo "=== what the listener now presents ==="
echo | openssl s_client -connect 127.0.0.1:9090 -servername "$HOST" 2>/dev/null \
	| openssl x509 -noout -subject -issuer -ext subjectAltName

echo
echo "Trust this file on each client: $WORK/ca.crt"
