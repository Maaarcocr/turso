# Turso MySQL conformance test Makefile
#
# The .sqltest corpora live here; the sqltest runner crate lives in
# testing/sqltest. The tursomysql backend runs Turso with the MySQL frontend
# in process, so only the runner needs to be built. See SPECS.md.

# Paths
ROOT_DIR := $(shell cd ../.. && pwd)
RUNNER_DIR := $(ROOT_DIR)/testing/sqltest

# Use release mode in CI, debug mode locally
ifdef CI
    CARGO_PROFILE := --release
else
    CARGO_PROFILE :=
endif

CARGO ?= cargo
SQLTEST := $(CARGO) run --manifest-path $(RUNNER_DIR)/Cargo.toml --bin sqltest $(CARGO_PROFILE) --
BACKEND := --backend tursomysql

# Test corpora (relative to this directory)
OWNED_TESTS := my-sqltests
SPECS := specs
# Specs materialized from committed results/ (never committed, see SPECS.md)
LOCAL_SPECS := $(SPECS)/local
# Committed spec directories: everything under specs/ except specs/local/
SPEC_DIRS = $(filter-out $(LOCAL_SPECS),$(wildcard $(SPECS)/*))
# Sources whose specs are rebuilt from results/<source>/ by materialize-local
LOCAL_SOURCES ?= $(notdir $(patsubst %/,%,$(dir $(wildcard results/*/meta.json))))
# Replay reports of the recorded specs, read by the scoreboard
REPORT_DIR := $(or $(CARGO_TARGET_DIR),$(ROOT_DIR)/target)
SPECS_REPORT ?= $(REPORT_DIR)/mysql-specs-report.json
LOCAL_SPECS_REPORT ?= $(REPORT_DIR)/mysql-local-specs-report.json

.PHONY: all build check run test run-specs run-one materialize-local run-local-specs clean

# Build everything
all: build

# Build the sqltest runner, which embeds the tursomysql backend
build:
	$(CARGO) build --manifest-path $(RUNNER_DIR)/Cargo.toml $(CARGO_PROFILE)

# Check syntax of the owned and committed recorded specs
check: build
	$(SQLTEST) check $(OWNED_TESTS) $(SPEC_DIRS)

# Run the owned specs; every one must pass
run: build
	$(SQLTEST) run $(OWNED_TESTS) $(BACKEND)

# Alias for run
test: run

# Replay every recorded spec and write a JSON report for the scoreboard.
# Failing specs are expected while the frontend grows, so they do not fail
# the target; the scoreboard compares the report with baseline.txt. Syntax
# errors are checked first because they also make the runner exit nonzero.
run-specs: build
	@if [ -n "$(SPEC_DIRS)" ]; then \
		$(SQLTEST) check $(SPEC_DIRS) && \
		{ $(SQLTEST) run $(SPEC_DIRS) $(BACKEND) --output json > $(SPECS_REPORT) || true; } && \
		echo "wrote $(SPECS_REPORT)"; \
	else \
		echo "no recorded specs in $(SPECS)/"; \
	fi

# Rebuild specs/local/<source>/ for sources that commit only MySQL's results
# (GPL or license-unclear test suites): fetch the pinned source, re-run its
# importer and join the committed results/<source>/ by id and SHA-256.
materialize-local:
	python3 -I tools/materialize.py $(LOCAL_SOURCES)

# Replay the materialized local specs; score with
# `python3 tools/score.py --local` (baseline-local.txt).
run-local-specs: build
	@if [ -d $(LOCAL_SPECS) ]; then \
		$(SQLTEST) check $(LOCAL_SPECS) && \
		{ $(SQLTEST) run $(LOCAL_SPECS) $(BACKEND) --output json > $(LOCAL_SPECS_REPORT) || true; } && \
		echo "wrote $(LOCAL_SPECS_REPORT)"; \
	else \
		echo "no local specs in $(LOCAL_SPECS)/; run make materialize-local"; \
	fi

# Run a single test file: make run-one FILE=my-sqltests/basics.sqltest
run-one: build
	$(SQLTEST) run $(FILE) $(BACKEND)

# Clean build artifacts
clean:
	$(CARGO) clean --manifest-path $(RUNNER_DIR)/Cargo.toml
