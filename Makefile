# Custom-API customs gateway: build and verification entry points.
#
# Everything routes through deploy/scripts/deploy.sh so that the Makefile and
# the shell tooling cannot drift apart.

SHELL := /bin/sh
DEPLOY := ./deploy/scripts/deploy.sh
# The release version is the workspace version. deploy.sh reads the same value
# from the same table, so the staged directory name and the CUSTOMS_VERSION
# stamped into the binaries cannot disagree.
VERSION := $(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -1)

.PHONY: help version all build stage ship install test clean distclean fmt vet \
        test-race test-cover local-test vm vm-baseline vm-status vm-console \
        vm-stop vm-destroy smoke intel e2e

# help prints the target map. The text lives in docs/MAKE-HELP.txt so that it
# cannot be mangled by shell or make quoting rules. The docs directory is
# migrated separately, so fall back to a short map instead of failing outright.
help:
	@if [ -f docs/MAKE-HELP.txt ]; then \
		cat docs/MAKE-HELP.txt; \
	else \
		echo "docs/MAKE-HELP.txt is not in place yet; targets:"; \
		echo "  build stage ship install test all local-test       (deploy/scripts/deploy.sh)"; \
		echo "  fmt vet test-race test-cover                        (cargo)"; \
		echo "  smoke intel e2e                                     (on the target)"; \
		echo "  vm vm-baseline vm-status vm-console vm-stop vm-destroy"; \
		echo "  clean distclean version"; \
	fi

# version prints what a build would stamp into the binaries.
version:
	@echo "$(VERSION)"

# --- build and deploy -------------------------------------------------------

all: build stage ship install test

build:
	@$(DEPLOY) build

stage:
	@$(DEPLOY) stage

ship:
	@$(DEPLOY) ship

install:
	@$(DEPLOY) install

test:
	@$(DEPLOY) test

# --- verification -----------------------------------------------------------

local-test:
	@$(DEPLOY) local-test

# rustfmt is the gofmt replacement: rewrite, then prove the tree is clean.
fmt:
	@cargo fmt --all && echo "formatted"
	@cargo fmt --all -- --check && echo "fmt clean"

# clippy is the go vet replacement. No `-D warnings`: PORTING.md section 5
# allows structural lints (too_many_arguments and friends), so this reports
# for review instead of blocking the release.
vet:
	@cargo clippy --workspace --all-targets --locked
	@echo "clippy finished; review any warnings above"

# Rust has no race detector to switch on, so this is simply the unit-test run.
# The historical target name is kept so that existing operator habits and the
# deployment docs keep working.
test-race:
	@cargo test --workspace --locked

# Coverage needs cargo-llvm-cov, which is not part of the pinned toolchain;
# fall back to the full test run rather than failing the target.
test-cover:
	@if cargo llvm-cov --version >/dev/null 2>&1; then \
		cargo llvm-cov --workspace --locked; \
	else \
		echo "cargo-llvm-cov is not installed; running the test suite instead"; \
		cargo test --workspace --locked; \
	fi

smoke:
	@ssh $${CUSTOMAPI_TARGET:-customapi-bsd} \
		'sudo -n sh /usr/local/share/customapi/smoke-test.sh'

intel:
	@ssh $${CUSTOMAPI_TARGET:-customapi-bsd} \
		'sudo -n sh /usr/local/share/customapi/intel-test.sh'

e2e: smoke intel

# --- test virtual machine ---------------------------------------------------

vm:
	@./vm/create-vm.sh

vm-baseline:
	@./vm/create-vm.sh baseline

vm-status:
	@./vm/create-vm.sh status

vm-console:
	@./vm/create-vm.sh console

vm-stop:
	@./vm/create-vm.sh stop

vm-destroy:
	@./vm/create-vm.sh destroy

# --- housekeeping -----------------------------------------------------------

# clean removes the release tree only. This is the housekeeping target, so the
# cargo build cache shared with test runs survives it; see the other half.
clean:
	@$(DEPLOY) clean
	@rm -rf dist
	@echo "cleaned"

# distclean also drops target/, which is minutes of recompilation and is
# therefore never removed implicitly.
distclean: clean
	@cargo clean
	@echo "cargo cache cleared"
