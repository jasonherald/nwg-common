# Makefile for nwg-common — library-subset per epic §3.6.
# No install / install-bin / install-data / upgrade / setup-* targets —
# the library doesn't install to a filesystem path; consumers pull it
# via `cargo add nwg-common`.

CARGO ?= cargo

.PHONY: all build build-release test lint check-tools \
        lint-fmt lint-clippy lint-deny lint-audit lint-docs \
        sonar clean help

all: build

help:
	@echo "Targets:"
	@echo "  make build          Debug build"
	@echo "  make build-release  Release build"
	@echo "  make test           cargo test + cargo clippy --all-targets"
	@echo "  make lint           Full local check: fmt + clippy + test + deny + audit + docs"
	@echo "  make sonar          Run SonarQube scan (requires sonar-scanner + .env token)"
	@echo "  make clean          cargo clean"

build:
	$(CARGO) build

build-release:
	$(CARGO) build --release

test:
	$(CARGO) test
	$(CARGO) clippy --all-targets

check-tools:
	@if ! command -v cargo-deny >/dev/null 2>&1; then \
		echo "Installing cargo-deny..."; \
		$(CARGO) install cargo-deny; \
	fi
	@if ! command -v cargo-audit >/dev/null 2>&1; then \
		echo "Installing cargo-audit..."; \
		$(CARGO) install cargo-audit; \
	fi

# Individual lint subtargets — each runnable on its own; `make lint`
# chains them so you can bisect a failing step (e.g. `make lint-clippy`).
lint-fmt:
	@echo "── Format ──"
	$(CARGO) fmt --all --check

lint-clippy:
	@echo "── Clippy ──"
	$(CARGO) clippy --all-targets -- -D warnings

lint-deny:
	@echo "── Cargo Deny (licenses, advisories, bans, sources) ──"
	$(CARGO) deny check

lint-audit:
	@echo "── Cargo Audit (dependency CVEs) ──"
	$(CARGO) audit

lint-docs:
	@echo "── Docs (missing-docs enforcement) ──"
	$(CARGO) rustdoc -- -D missing-docs

lint: check-tools lint-fmt lint-clippy test lint-deny lint-audit lint-docs
	@echo ""
	@echo "All local checks passed ✓"

sonar:
	@echo "Running SonarQube scan..."
	@. ./.env && export SONAR_TOKEN && \
	SONAR_SCANNER_OPTS="-Djavax.net.ssl.trustStore=/tmp/sonar-truststore.jks -Djavax.net.ssl.trustStorePassword=changeit" \
	/opt/sonar-scanner/bin/sonar-scanner \
		-Dsonar.host.url=https://sonar.aaru.network

clean:
	$(CARGO) clean
