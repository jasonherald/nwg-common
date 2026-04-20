# Makefile for nwg-common — library-subset per epic §3.6.
# No install / install-bin / install-data / upgrade / setup-* targets —
# the library doesn't install to a filesystem path; consumers pull it
# via `cargo add nwg-common`.

CARGO ?= cargo
SONAR_SCANNER ?= /opt/sonar-scanner/bin/sonar-scanner
SONAR_HOST_URL ?= https://sonar.aaru.network
SONAR_TRUSTSTORE ?= /tmp/sonar-truststore.jks
SONAR_TRUSTSTORE_PASSWORD ?= changeit

.PHONY: all build build-release test lint check-tools \
        lint-fmt lint-clippy lint-test lint-deny lint-audit lint-docs \
        sonar clean help

all: build

define HELP_TEXT
Targets:
  make build          Debug build
  make build-release  Release build
  make test           cargo test + cargo clippy --all-targets
  make lint           Full local check: fmt + clippy + test + deny + audit + docs
  make sonar          Run SonarQube scan (requires sonar-scanner + .env token)
  make clean          cargo clean
endef
export HELP_TEXT

help:
	@echo "$$HELP_TEXT"

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

# Plain test run — no clippy, unlike the top-level `test` target.
# `lint` composes this (instead of `test`) so clippy runs exactly
# once per `make lint`, via `lint-clippy`.
lint-test:
	@echo "── Tests ──"
	$(CARGO) test

lint-deny:
	@echo "── Cargo Deny (licenses, advisories, bans, sources) ──"
	$(CARGO) deny check

lint-audit:
	@echo "── Cargo Audit (dependency CVEs) ──"
	$(CARGO) audit

lint-docs:
	@echo "── Docs (missing-docs enforcement) ──"
	$(CARGO) rustdoc -- -D missing-docs

lint: check-tools lint-fmt lint-clippy lint-test lint-deny lint-audit lint-docs
	@echo ""
	@echo "All local checks passed ✓"

# Parse SONAR_TOKEN out of .env rather than sourcing the file — a
# sourced .env executes any shell code it contains, which is a real
# injection risk if a contributor ever saves it with an unintended
# command or a value that contains backticks / $(…). awk extracts
# just the value.
sonar:
	@echo "Running SonarQube scan..."
	@test -f ./.env || { echo "ERROR: .env not found in repo root"; exit 1; }
	@command -v "$(SONAR_SCANNER)" >/dev/null 2>&1 || [ -x "$(SONAR_SCANNER)" ] || { \
		echo "ERROR: sonar-scanner not found (looked at $(SONAR_SCANNER))"; exit 1; \
	}
	@test -r "$(SONAR_TRUSTSTORE)" || { \
		echo "ERROR: truststore not found or not readable at $(SONAR_TRUSTSTORE)"; \
		echo "  (sonar.aaru.network uses a self-signed cert — regenerate with:"; \
		echo "     openssl s_client -connect sonar.aaru.network:443 -showcerts </dev/null 2>/dev/null \\\\"; \
		echo "       | awk '/BEGIN CERT/,/END CERT/' > /tmp/sonar-cert.pem && \\\\"; \
		echo "     keytool -importcert -alias sonar-aaru -file /tmp/sonar-cert.pem \\\\"; \
		echo "       -keystore $(SONAR_TRUSTSTORE) -storepass $(SONAR_TRUSTSTORE_PASSWORD) -noprompt)"; \
		exit 1; \
	}
	@TOKEN="$$(awk '/^SONAR_TOKEN=/{sub(/^[^=]*=[ \t]*/, ""); sub(/[ \t]+$$/, ""); print; exit}' ./.env)"; \
	test -n "$$TOKEN" || { echo "ERROR: SONAR_TOKEN is empty in .env"; exit 1; }; \
	SONAR_TOKEN="$$TOKEN" \
	SONAR_SCANNER_OPTS="-Djavax.net.ssl.trustStore=$(SONAR_TRUSTSTORE) -Djavax.net.ssl.trustStorePassword=$(SONAR_TRUSTSTORE_PASSWORD)" \
	"$(SONAR_SCANNER)" -Dsonar.host.url="$(SONAR_HOST_URL)"

clean:
	$(CARGO) clean
