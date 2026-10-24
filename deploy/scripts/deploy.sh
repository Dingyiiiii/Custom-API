#!/bin/sh
#
# Custom-API customs gateway: build and deploy driver.
#
# Runs on the development host (or on any machine with the Rust toolchain) and
# produces a FreeBSD release, ships it to a target, installs it and runs the
# end-to-end acceptance test.
#
# Usage:
#   ./deploy/scripts/deploy.sh build            cross-compile the FreeBSD release
#   ./deploy/scripts/deploy.sh stage            assemble dist/customapi-<version>/
#   ./deploy/scripts/deploy.sh ship             copy the release to the target
#   ./deploy/scripts/deploy.sh install          run the installer on the target
#   ./deploy/scripts/deploy.sh test             run the acceptance test on the target
#   ./deploy/scripts/deploy.sh all              build + stage + ship + install + test
#   ./deploy/scripts/deploy.sh local-test       run cargo fmt/clippy/test on the host
#   ./deploy/scripts/deploy.sh clean            remove build output
#
# Target selection, in order of precedence:
#   --target USER@HOST   explicit
#   $CUSTOMAPI_TARGET    environment
#   customapi-bsd        the SSH alias created for the libvirt test VM
#
# The FreeBSD test VM is managed with libvirt; see docs/VM-zh.md.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
CARGO_MANIFEST="${ROOT}/Cargo.toml"
WEB="${ROOT}/web"
DIST="${ROOT}/dist"
# The release version is the workspace version; the sed only looks inside the
# [workspace.package] table so a dependency elsewhere cannot be mistaken for
# it. The crates default to "0.1.0-dev" when CUSTOMS_VERSION is absent, so an
# unstamped build announces itself instead of claiming a release number.
VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "${CARGO_MANIFEST}" 2>/dev/null | head -1)
[ -n "${VERSION}" ] || VERSION=dev
COMMIT=$(git -C "${ROOT}" rev-parse --short HEAD 2>/dev/null || echo unknown)
BUILT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
STAGE="${DIST}/customapi-${VERSION}"
TARGET=${CUSTOMAPI_TARGET:-customapi-bsd}
REMOTE_DIR=/tmp/customapi-release
# The FreeBSD triple the release is built for. It comes from `rustup target
# add`; override only when deliberately targeting a different architecture.
RUST_TARGET=${CUSTOMS_RUST_TARGET:-x86_64-unknown-freebsd}
# Filled in by build, read by stage. Empty means "not built in this run".
BUILD_TRIPLE=""

# Deliberately quiet: the deploy log should show outcomes, not command echo.
log()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require() {
	command -v "$1" >/dev/null 2>&1 || die "$1 is required but not found in PATH"
}

# rustc reports its own host triple, which is also the triple of a build made
# without --target.
host_triple() {
	rustc -vV 2>/dev/null | sed -n 's/^host: //p' | head -1
}

rustc_version() {
	rustc --version 2>/dev/null | awk '{print $2}'
}

# A rustc installed from a distribution package has no rustup and cannot add
# targets at all; that is reported exactly like a missing target, because the
# consequence is the same.
target_installed() {
	command -v rustup >/dev/null 2>&1 || return 1
	rustup target list --installed 2>/dev/null | grep -qx "${RUST_TARGET}"
}

# ---------------------------------------------------------------------------
# build
# ---------------------------------------------------------------------------

do_build() {
	require cargo
	require rustc

	BUILD_TRIPLE="${RUST_TARGET}"
	TARGET_ARG="--target ${RUST_TARGET}"
	if ! target_installed; then
		# Delivering a FreeBSD release is the point of this script, but a
		# half-provisioned host must not be a dead end: name the missing piece
		# precisely, then build natively so that stage/ship dry runs and local
		# acceptance work stay possible. RELEASE records the triple actually
		# used, so a host build can never be mistaken for a FreeBSD binary.
		BUILD_TRIPLE=$(host_triple)
		[ -n "${BUILD_TRIPLE}" ] || BUILD_TRIPLE=$(uname -srm | tr ' ' '-')
		TARGET_ARG=""
		warn "${RUST_TARGET} is not installed for this toolchain"
		warn "  install it with: rustup target add ${RUST_TARGET}"
		warn "  cross-linking also needs a compiler for the target: the rustls"
		warn "  stack builds ring's C and assembly, so export"
		warn "  CC_$(printf '%s' "${RUST_TARGET}" | tr '-' '_')=<freebsd-cc>, or"
		warn "  use cargo-zigbuild, or build on the FreeBSD target itself (the"
		warn "  VM baseline installs the rust package for exactly this)"
		warn "falling back to a native build for ${BUILD_TRIPLE}: the binaries will NOT run on FreeBSD"
	fi

	log "building customs ${VERSION} for ${BUILD_TRIPLE}"
	mkdir -p "${DIST}/bin"

	# cargo writes to target/release/ for a native build and to
	# target/<triple>/release/ for a cross build; staging needs the one used.
	OUT_DIR="${ROOT}/target/release"
	if [ -n "${TARGET_ARG}" ]; then
		OUT_DIR="${ROOT}/target/${RUST_TARGET}/release"
	fi

	# Build identity. Go injected this with `-ldflags -X`; the Rust crates read
	# the same three values (plus the rustc version) through option_env!, i.e.
	# at compile time, so they must be in the environment of the cargo call.
	CUSTOMS_VERSION="${VERSION}"
	CUSTOMS_COMMIT="${COMMIT}"
	CUSTOMS_BUILD_TIME="${BUILT}"
	CUSTOMS_RUSTC=$(rustc_version)
	export CUSTOMS_VERSION CUSTOMS_COMMIT CUSTOMS_BUILD_TIME CUSTOMS_RUSTC

	# option_env! is invisible to cargo's fingerprint: without dropping the core
	# crate's artifacts, a rebuild after a version bump would silently keep the
	# previous stamp and the release would ship binaries that misreport
	# themselves. Only the release artifacts of that one crate are dropped, so
	# dependencies stay cached and an unrelated `cargo test` keeps its dev build.
	(cd "${ROOT}" && cargo clean --quiet --release -p customapi-core)

	# Strip the release the way `-s -w` stripped the Go binaries. Passed as a
	# profile override because Cargo.toml is maintained by the core owners, and
	# the clean above already forces a relink.
	log "  customsd benchd customsctl"
	(cd "${ROOT}" && cargo build --release --locked ${TARGET_ARG} \
		--config 'profile.release.strip="symbols"' \
		-p customsd -p benchd -p customsctl)

	for cmd in customsd benchd customsctl; do
		[ -f "${OUT_DIR}/${cmd}" ] || die "cargo did not produce ${OUT_DIR}/${cmd}"
		install -m 0755 "${OUT_DIR}/${cmd}" "${DIST}/bin/${cmd}"
	done

	# Record the triple next to the binaries. `stage` is often run as a separate
	# invocation, and the release metadata must describe the binaries that were
	# actually copied, not the host that happens to be staging them.
	printf '%s\n' "${BUILD_TRIPLE}" > "${DIST}/.build-triple"

	# Also verify the workspace builds for the host, so a developer does not
	# discover a platform-specific problem only at deploy time. Only meaningful
	# after a cross build; the native path above has just proved it.
	if [ -n "${TARGET_ARG}" ]; then
		log "  native build check"
		(cd "${ROOT}" && cargo check --quiet --locked --workspace)
	fi

	log "binaries:"
	# Field positions in `ls -l` depend on the locale's date format, so the
	# listing is assembled from the paths instead of from awk's $9/$5.
	for cmd in customsd benchd customsctl; do
		printf '  %-12s %8s bytes\n' "${cmd}" "$(wc -c < "${DIST}/bin/${cmd}" | tr -d ' ')"
	done
}

# ---------------------------------------------------------------------------
# stage
# ---------------------------------------------------------------------------

do_stage() {
	[ -x "${DIST}/bin/customsd" ] || die "nothing to stage; run 'build' first"
	log "staging ${STAGE}"
	rm -rf "${STAGE}"
	mkdir -p "${STAGE}/bin" "${STAGE}/etc" "${STAGE}/rc.d" "${STAGE}/scripts" "${STAGE}/docs"

	cp "${DIST}/bin/customsd" "${DIST}/bin/benchd" "${DIST}/bin/customsctl" "${STAGE}/bin/"
	cp "${ROOT}/deploy/etc/customs.json"           "${STAGE}/etc/"
	cp "${ROOT}/deploy/etc/customsd.env.example"   "${STAGE}/etc/"
	cp "${ROOT}/deploy/rc.d/customsd"              "${STAGE}/rc.d/"
	cp "${ROOT}/deploy/rc.d/benchd"                "${STAGE}/rc.d/"
	cp "${ROOT}/deploy/scripts/install.sh"         "${STAGE}/scripts/"
	cp "${ROOT}/deploy/scripts/smoke-test.sh"      "${STAGE}/scripts/"
	cp "${ROOT}/deploy/scripts/mock-upstream.py"   "${STAGE}/scripts/"
	cp "${ROOT}/deploy/scripts/customsd-wrapper.sh" "${STAGE}/scripts/"
	cp "${ROOT}/deploy/scripts/intel-test.sh"       "${STAGE}/scripts/"
	cp "${ROOT}/deploy/scripts/verify-deployment.sh" "${STAGE}/scripts/"

	# The built front end travels with the release. A missing build is not an
	# error: the gateway serves its own placeholder explaining how to produce
	# one, so a backend-only deployment still works. The placeholder is
	# deliberately not shipped: it would hide the missing `npm run build`.
	if [ -f "${WEB}/dist/index.html" ]; then
		log "  including the built web UI"
		mkdir -p "${STAGE}/web"
		cp -r "${WEB}/dist/." "${STAGE}/web/"
	else
		warn "the web UI has not been built; the gateway will serve a placeholder"
		warn "  build it with: cd web && npm ci && npm run build"
	fi
	for f in "${ROOT}"/docs/*.md; do
		[ -f "$f" ] && cp "$f" "${STAGE}/docs/"
	done

	# `stage` is usually a separate invocation from `build`, so the triple comes
	# from the stamp the build left beside the binaries; only a hand-assembled
	# dist/bin needs the host triple as a fallback.
	RELEASE_TARGET=${BUILD_TRIPLE:-$(cat "${DIST}/.build-triple" 2>/dev/null || true)}
	[ -n "${RELEASE_TARGET}" ] || RELEASE_TARGET=$(host_triple)
	[ -n "${RELEASE_TARGET}" ] || RELEASE_TARGET=unknown

	cat > "${STAGE}/RELEASE" <<EOF
component: customapi-customs
version:   ${VERSION}
commit:    ${COMMIT}
built:     ${BUILT}
target:    ${RELEASE_TARGET}
rustc:     $(rustc_version)
EOF

	# Checksums so an operator can verify a copied release.
	(cd "${STAGE}" && find . -type f ! -name SHA256SUMS -exec sha256sum {} \; | sort -k2 > SHA256SUMS)

	log "release contents:"
	(cd "${STAGE}" && find . -type f | sort | sed 's/^/  /')
}

# ---------------------------------------------------------------------------
# ship / install / test
# ---------------------------------------------------------------------------

do_ship() {
	[ -d "${STAGE}" ] || die "nothing to ship; run 'stage' first"
	require ssh
	require scp
	log "shipping ${STAGE} to ${TARGET}:${REMOTE_DIR}"
	ssh -o BatchMode=yes "${TARGET}" "rm -rf ${REMOTE_DIR} && mkdir -p ${REMOTE_DIR}" \
		|| die "cannot reach ${TARGET} over SSH; is the VM running?"
	scp -q -r "${STAGE}/." "${TARGET}:${REMOTE_DIR}/"
	log "copied $(ssh "${TARGET}" "find ${REMOTE_DIR} -type f | wc -l | tr -d ' '") files"
}

do_install() {
	log "installing on ${TARGET}"
	ssh "${TARGET}" "sudo -n sh ${REMOTE_DIR}/scripts/install.sh" \
		|| die "installation failed on ${TARGET}"
}

do_test() {
	log "running the acceptance test on ${TARGET}"
	ssh "${TARGET}" "sudo -n sh ${REMOTE_DIR}/scripts/smoke-test.sh" \
		|| die "the acceptance test reported failures"
}

# ---------------------------------------------------------------------------
# local-test
# ---------------------------------------------------------------------------

do_local_test() {
	require cargo
	log "cargo fmt --check (the gofmt gate)"
	(cd "${ROOT}" && cargo fmt --all -- --check)
	# No `-D warnings`: PORTING.md section 5 allows structural lints such as
	# too_many_arguments, so clippy informs the review instead of blocking it.
	log "cargo clippy (the go vet gate)"
	(cd "${ROOT}" && cargo clippy --workspace --all-targets --locked)
	log "cargo test (replaces go test -race; Rust has no race detector to switch on)"
	(cd "${ROOT}" && cargo test --workspace --locked)
	if command -v cargo-llvm-cov >/dev/null 2>&1; then
		log "cargo llvm-cov (replaces go test -cover)"
		(cd "${ROOT}" && cargo llvm-cov --workspace --locked)
	else
		warn "cargo-llvm-cov is not installed; the test run above is the only coverage gate"
	fi
}

do_clean() {
	# target/ is cargo's cache and is shared with unrelated development and
	# test runs, so it is kept: deleting it turns the next build into minutes
	# of recompilation. `make distclean` drops it explicitly.
	log "removing ${DIST}"
	rm -rf "${DIST}"
}

# ---------------------------------------------------------------------------

CMD=${1:-all}
if [ $# -gt 0 ]; then shift; fi
while [ $# -gt 0 ]; do
	case "$1" in
	--target) TARGET="$2"; shift 2 ;;
	--version) VERSION="$2"; STAGE="${DIST}/customapi-${VERSION}"; shift 2 ;;
	*) die "unknown option: $1" ;;
	esac
done

case "${CMD}" in
build)      do_build ;;
stage)      do_stage ;;
ship)       do_ship ;;
install)    do_install ;;
test)       do_test ;;
all)        do_build; do_stage; do_ship; do_install; do_test ;;
local-test) do_local_test ;;
clean)      do_clean ;;
*)
	sed -n '2,25p' "$0"
	exit 2
	;;
esac

log "done: ${CMD}"
