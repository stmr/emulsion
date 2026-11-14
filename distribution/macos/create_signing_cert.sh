#!/usr/bin/env bash
# Create a local code signing identity so macOS remembers Emulsion's permissions.
#
#   ./distribution/macos/create_signing_cert.sh
#   ./distribution/macos/create_dmg.sh
#
# macOS keys "Files and Folders" / "Full Disk Access" grants to the app's code signature.
# An ad-hoc signature is just a hash of the binary, so every rebuild looks like a different
# app and the permission prompt comes back. A certificate keeps the identity stable.
#
# The certificate is self-signed and trusted for code signing for this user only; macOS asks
# for your login password while adding that trust setting. Nothing is sent anywhere.

set -euo pipefail

readonly IDENTITY="${CODESIGN_IDENTITY:-Emulsion Local Signing}"
readonly KEYCHAIN="${KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

fail() {
	echo "error: $*" >&2
	exit 1
}

[[ "$(uname -s)" == "Darwin" ]] || fail "this script must run on macOS (Darwin)"

if security find-identity -v -p codesigning "$KEYCHAIN" | grep -qF "$IDENTITY"; then
	echo "Signing identity '${IDENTITY}' already exists in ${KEYCHAIN}"
	echo "Build with: CODESIGN_IDENTITY='${IDENTITY}' ./distribution/macos/create_dmg.sh"
	exit 0
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/emulsion-cert.XXXXXX")"
cleanup() {
	rm -rf "$work_dir"
}
trap cleanup EXIT

# Stock macOS ships LibreSSL, whose `pkcs12 -export` already writes the legacy encryption that
# `security import` needs and has no `-legacy` option. OpenSSL 3+ (e.g. from Homebrew) defaults
# to modern encryption and needs `-legacy`.
if [[ -x /usr/bin/openssl ]]; then
	OPENSSL=/usr/bin/openssl
else
	OPENSSL=openssl
fi
pkcs12_legacy=()
if "$OPENSSL" pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
	pkcs12_legacy=(-legacy)
fi

# Run openssl quietly, but show its output when it fails.
run_openssl() {
	"$OPENSSL" "$@" 2>"${work_dir}/openssl.log" || {
		cat "${work_dir}/openssl.log" >&2
		return 1
	}
}

run_openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
	-keyout "${work_dir}/key.pem" -out "${work_dir}/cert.pem" \
	-subj "/CN=${IDENTITY}" \
	-addext "basicConstraints=critical,CA:false" \
	-addext "keyUsage=critical,digitalSignature" \
	-addext "extendedKeyUsage=critical,codeSigning" \
	|| fail "could not create the certificate with openssl"

run_openssl pkcs12 -export ${pkcs12_legacy[@]+"${pkcs12_legacy[@]}"} -macalg sha1 \
	-inkey "${work_dir}/key.pem" -in "${work_dir}/cert.pem" \
	-out "${work_dir}/identity.p12" -passout pass:emulsion -name "$IDENTITY" \
	|| fail "could not package the certificate as PKCS#12"

security import "${work_dir}/identity.p12" -k "$KEYCHAIN" -P emulsion -T /usr/bin/codesign \
	|| fail "could not import the identity into ${KEYCHAIN}"
if ! security set-key-partition-list -S apple-tool:,apple: -s -k "" "$KEYCHAIN" >/dev/null; then
	echo "warning: could not let codesign use the new key without prompting" >&2
	echo "  (set-key-partition-list needs the keychain password; it is empty here)." >&2
	echo "  macOS may ask for your login password when signing; choose 'Always Allow'." >&2
fi

echo "Trusting '${IDENTITY}' for code signing (macOS will ask for your login password)"
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "${work_dir}/cert.pem" \
	|| fail "could not trust the certificate for code signing"

security find-identity -v -p codesigning "$KEYCHAIN" | grep -F "$IDENTITY" \
	|| fail "the identity is not usable for code signing"

echo
echo "Done. Rebuild with:"
echo "  ./distribution/macos/create_dmg.sh"
echo "Grant the folder access once after installing that build; it now survives rebuilds."
