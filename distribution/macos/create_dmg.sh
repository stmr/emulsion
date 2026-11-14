#!/usr/bin/env bash
# Build a release AVIF-enabled Universal 2 app and DMG from the repository root:
#   ./distribution/macos/create_dmg.sh

set -euo pipefail

readonly APP_NAME="Emulsion"
readonly BINARY_NAME="emulsion"
readonly ARM64_TARGET="aarch64-apple-darwin"
readonly X86_64_TARGET="x86_64-apple-darwin"
readonly MACOS_DEPLOYMENT_TARGET="11.0"
readonly DAV1D_VERSION="1.3.0"
readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly OUTPUT_DMG="${OUTPUT_DMG:-${REPO_ROOT}/${APP_NAME}.dmg}"
readonly UNIVERSAL_TARGET_DIR="${REPO_ROOT}/target/universal/release"
readonly ARM64_BINARY="${REPO_ROOT}/target/${ARM64_TARGET}/release/${BINARY_NAME}"
readonly X86_64_BINARY="${REPO_ROOT}/target/${X86_64_TARGET}/release/${BINARY_NAME}"
readonly APP_BUNDLE="${UNIVERSAL_TARGET_DIR}/bundle/osx/${APP_NAME}.app"
readonly APP_BINARY="${APP_BUNDLE}/Contents/MacOS/${BINARY_NAME}"
readonly APP_PLIST="${APP_BUNDLE}/Contents/Info.plist"

usage() {
	cat <<EOF
Usage:
  ./distribution/macos/create_dmg.sh

Builds release AVIF-enabled ${APP_NAME}.app for ${ARM64_TARGET} and
${X86_64_TARGET}, combines only the executable into a Universal 2 binary,
verifies the bundle, signs it ad hoc, and writes ${APP_NAME}.dmg at the
repository root (or at OUTPUT_DMG when set).
EOF
}

fail() {
	echo "error: $*" >&2
	exit 1
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
	usage
	exit 0
fi
if [[ "$#" -ne 0 ]]; then
	usage >&2
	fail "unexpected argument: $1"
fi

[[ "$(uname -s)" == "Darwin" ]] || fail "this build must run on macOS (Darwin)"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

required_tools=(
	cargo
	cargo-bundle
	rustup
	lipo
	file
	otool
	plutil
	codesign
	create-dmg
	awk
	git
	meson
	ninja
	nasm
	pkg-config
	ditto
)
for tool in "${required_tools[@]}"; do
	command -v "$tool" >/dev/null 2>&1 || fail "required tool '${tool}' was not found on PATH"
done

installed_targets="$(rustup target list --installed)" || fail "could not query installed Rust targets with rustup"
for target in "$ARM64_TARGET" "$X86_64_TARGET"; do
	target_installed=false
	while IFS= read -r installed_target; do
		if [[ "$installed_target" == "$target" ]]; then
			target_installed=true
			break
		fi
	done <<< "$installed_targets"
	if [[ "$target_installed" != true ]]; then
		fail "Rust target '${target}' is not installed; run 'rustup target add ${target}' first"
	fi
done

cd "$REPO_ROOT"
export MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET"
export SYSTEM_DEPS_DAV1D_LINK=static
export CARGO_INCREMENTAL=0

echo "Building ${APP_NAME} Universal 2 bundle"
echo "  targets: ${ARM64_TARGET}, ${X86_64_TARGET}"
echo "  features: avif"
echo "  MACOSX_DEPLOYMENT_TARGET=${MACOS_DEPLOYMENT_TARGET}"
echo "  SYSTEM_DEPS_DAV1D_LINK=${SYSTEM_DEPS_DAV1D_LINK}"
echo "  dav1d: ${DAV1D_VERSION} (static, per-slice)"

# dav1d is a C library, so each slice needs its own build. `dav1d-sys` can only
# build it for the host, hence the explicit meson cross build for the Intel slice.
DAV1D_PREFIX="${REPO_ROOT}/target/dav1d/${X86_64_TARGET}"
DAV1D_SOURCE="${REPO_ROOT}/target/dav1d/src"
if [[ ! -f "${DAV1D_PREFIX}/lib/pkgconfig/dav1d.pc" ]]; then
	if [[ ! -d "${DAV1D_SOURCE}/.git" ]]; then
		rm -rf "$DAV1D_SOURCE"
		mkdir -p "$(dirname -- "$DAV1D_SOURCE")"
		git clone --depth 1 -b "$DAV1D_VERSION" https://code.videolan.org/videolan/dav1d.git "$DAV1D_SOURCE"
	fi
	cross_file="${REPO_ROOT}/target/dav1d/${X86_64_TARGET}-cross.ini"
	mkdir -p "$(dirname -- "$cross_file")"
	cat >"$cross_file" <<EOF
[binaries]
c = ['clang', '-arch', 'x86_64', '-mmacosx-version-min=${MACOS_DEPLOYMENT_TARGET}']
cpp = ['clang++', '-arch', 'x86_64', '-mmacosx-version-min=${MACOS_DEPLOYMENT_TARGET}']
ar = 'ar'
strip = 'strip'
nasm = 'nasm'
pkg-config = 'pkg-config'

[host_machine]
system = 'darwin'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
	rm -rf "${REPO_ROOT}/target/dav1d/${X86_64_TARGET}-build"
	meson setup \
		-Ddefault_library=static \
		--cross-file "$cross_file" \
		--prefix "$DAV1D_PREFIX" \
		"${REPO_ROOT}/target/dav1d/${X86_64_TARGET}-build" \
		"$DAV1D_SOURCE"
	ninja -C "${REPO_ROOT}/target/dav1d/${X86_64_TARGET}-build"
	meson install -C "${REPO_ROOT}/target/dav1d/${X86_64_TARGET}-build"
fi
[[ -f "${DAV1D_PREFIX}/lib/pkgconfig/dav1d.pc" ]] \
	|| fail "could not build dav1d for ${X86_64_TARGET}"

PKG_CONFIG_ALLOW_CROSS=1 \
	PKG_CONFIG_PATH="${DAV1D_PREFIX}/lib/pkgconfig" \
	cargo build --release --locked --features=avif --target="${X86_64_TARGET}"
# `cargo bundle` has no --locked flag, so build with --locked first and make bundling skip
# its own (unlocked) `cargo build`.
SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always \
	cargo build --release --locked --features=avif --target="${ARM64_TARGET}"
CARGO_BUNDLE_SKIP_BUILD=1 \
	cargo bundle --release --format osx --features=avif --target="${ARM64_TARGET}"

ARM64_BUNDLE="${REPO_ROOT}/target/${ARM64_TARGET}/release/bundle/osx/${APP_NAME}.app"
[[ -f "$ARM64_BINARY" ]] || fail "cargo bundle did not produce ${ARM64_BINARY}"
[[ -f "$X86_64_BINARY" ]] || fail "cargo build did not produce ${X86_64_BINARY}"
[[ -d "$ARM64_BUNDLE" ]] || fail "cargo bundle did not produce ${ARM64_BUNDLE}"

rm -rf "$APP_BUNDLE"
mkdir -p "$(dirname -- "$APP_BUNDLE")"
ditto "$ARM64_BUNDLE" "$APP_BUNDLE"
lipo -create -output "$APP_BINARY" "$ARM64_BINARY" "$X86_64_BINARY"

[[ -f "$APP_BINARY" ]] || fail "bundle executable is missing: ${APP_BINARY}"
[[ -d "${APP_BUNDLE}/Contents/Resources" ]] || fail "bundle resources are missing: ${APP_BUNDLE}/Contents/Resources"
[[ -f "$APP_PLIST" ]] || fail "bundle Info.plist is missing: ${APP_PLIST}"

verify_architecture() {
	local binary="$1"
	local expected_arch="$2"
	local actual_arches
	actual_arches="$(lipo -archs "$binary")" || fail "could not inspect architectures in ${binary}"
	if [[ "$expected_arch" == "arm64 x86_64" ]]; then
		[[ "$actual_arches" == "arm64 x86_64" || "$actual_arches" == "x86_64 arm64" ]] \
			|| fail "${binary} has architecture(s) '${actual_arches}', expected arm64 and x86_64"
	elif [[ "$actual_arches" != "$expected_arch" ]]; then
		fail "${binary} has architecture(s) '${actual_arches}', expected '${expected_arch}'"
	fi
}

verify_architecture "$ARM64_BINARY" arm64
verify_architecture "$X86_64_BINARY" x86_64
verify_architecture "$APP_BINARY" "arm64 x86_64"

actual_arches="$(lipo -archs "$APP_BINARY")"
echo "Universal executable architectures: ${actual_arches}"
file "$APP_BINARY"

plutil -lint "$APP_PLIST" >/dev/null || fail "bundle Info.plist failed validation: ${APP_PLIST}"
plutil -extract CFBundleDocumentTypes xml1 -o /dev/null "$APP_PLIST" >/dev/null 2>&1 \
	|| fail "bundle Info.plist is missing CFBundleDocumentTypes"
plutil -extract UTImportedTypeDeclarations xml1 -o /dev/null "$APP_PLIST" >/dev/null 2>&1 \
	|| fail "bundle Info.plist is missing UTImportedTypeDeclarations"

deployment_versions="$(otool -l "$APP_BINARY" | awk '
	/LC_BUILD_VERSION/ { in_build = 1; next }
	in_build && /minos/ { print $2; in_build = 0; next }
	/LC_VERSION_MIN_MACOSX/ { in_legacy = 1; next }
	in_legacy && /version/ { print $2; in_legacy = 0 }
')"
version_count=0
while IFS= read -r deployment_version; do
	[[ -z "$deployment_version" ]] && continue
	version_count=$((version_count + 1))
	[[ "$deployment_version" == "$MACOS_DEPLOYMENT_TARGET" ]] \
		|| fail "bundle executable has macOS deployment target ${deployment_version}, expected ${MACOS_DEPLOYMENT_TARGET}"
done <<< "$deployment_versions"
[[ "$version_count" -ge 2 ]] || fail "could not verify macOS deployment target for both Universal 2 slices"

# TCC (the "Emulsion would like to access ..." prompt) remembers the grant per code
# signature. An ad-hoc signature is only a cdhash, so every rebuild looks like a new app and
# asks again; sign with a certificate (CODESIGN_IDENTITY) to keep the grant across rebuilds.
readonly LOCAL_IDENTITY="Emulsion Local Signing"
if [[ -z "${CODESIGN_IDENTITY:-}" ]] \
	&& security find-identity -v -p codesigning | grep -qF "$LOCAL_IDENTITY"; then
	CODESIGN_IDENTITY="$LOCAL_IDENTITY"
fi
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
	echo "Ad-hoc signing ${APP_BUNDLE}"
	echo "  file access has to be granted again after each build;"
	echo "  run ./distribution/macos/create_signing_cert.sh once to keep it"
else
	echo "Signing ${APP_BUNDLE} with ${CODESIGN_IDENTITY}"
fi
codesign --force --deep --sign "$CODESIGN_IDENTITY" "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"
codesign --display --requirements - "$APP_BUNDLE" 2>&1 | grep 'designated' || true
verify_architecture "$APP_BINARY" "arm64 x86_64"

mkdir -p "$(dirname -- "$OUTPUT_DMG")"
rm -f "$OUTPUT_DMG"
dmg_staging_dir="$(mktemp -d "${TMPDIR:-/tmp}/emulsion-dmg.XXXXXX")"
cleanup() {
	rm -rf "$dmg_staging_dir"
}
trap cleanup EXIT

create-dmg \
	--overwrite \
	--no-version-in-filename \
	--no-code-sign \
	--dmg-title="$APP_NAME" \
	"$APP_BUNDLE" \
	"$dmg_staging_dir"

created_dmg="${dmg_staging_dir}/${APP_NAME}.dmg"
[[ -f "$created_dmg" ]] || fail "create-dmg did not produce ${created_dmg}"
mv "$created_dmg" "$OUTPUT_DMG"
echo "Created ${OUTPUT_DMG}"
