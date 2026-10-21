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
    TARGET_DIR := release
else
    CARGO_PROFILE :=
    TARGET_DIR := debug
endif

CARGO ?= cargo
SQLTEST := $(CARGO) run --manifest-path $(RUNNER_DIR)/Cargo.toml --bin sqltest $(CARGO_PROFILE) --
BACKEND := --backend tursomysql

# Honor a CARGO_TARGET_DIR override; default to the workspace target dir.
TARGET_ROOT := $(or $(CARGO_TARGET_DIR),$(ROOT_DIR)/target)
SQLTEST_BIN := $(TARGET_ROOT)/$(TARGET_DIR)/sqltest

# Test corpora (relative to this directory)
OWNED_TESTS := my-sqltests
SPECS := specs
# Specs materialized from committed results/ (ignored by git, see SPECS.md)
LOCAL_SPECS := $(SPECS)/local
# Committed recorded specs: every source under specs/ except specs/local/
COMMITTED_SPECS := $(filter-out $(LOCAL_SPECS),$(patsubst %/,%,$(wildcard $(SPECS)/*/)))
# Recorded specs to replay: every committed source, plus the ignored local
# specs with INCLUDE_LOCAL=1.
SPECS_DIRS := $(COMMITTED_SPECS)
ifdef INCLUDE_LOCAL
    SPECS_DIRS += $(wildcard $(LOCAL_SPECS))
    REPLAY_LOCAL := --include-local
endif
# Sources whose specs materialize-local rebuilds from results/<source>/
LOCAL_SOURCES ?= $(notdir $(patsubst %/,%,$(dir $(wildcard results/*/meta.json))))
# Replay reports of the recorded specs, read by the scoreboard
SPECS_REPORT ?= $(TARGET_ROOT)/mysql-specs-report.json
LOCAL_SPECS_REPORT ?= $(TARGET_ROOT)/mysql-local-specs-report.json
# sqltest processes at a time, seconds per setup or test block, and the
# address-space limit of each process (see tools/replay.py)
SPECS_JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)
SPECS_TIMEOUT ?= 30
SPECS_MEMORY_MB ?= 2048
REPLAY_OPTIONS = --sqltest $(SQLTEST_BIN) --jobs $(SPECS_JOBS) --timeout $(SPECS_TIMEOUT) \
	--memory-mb $(SPECS_MEMORY_MB)

.PHONY: all build check run test run-specs score triage run-one materialize-local run-local-specs \
	score-local clean

# Build everything
all: build

# Build the sqltest runner, which embeds the tursomysql backend
build:
	$(CARGO) build --manifest-path $(RUNNER_DIR)/Cargo.toml $(CARGO_PROFILE)

# Check syntax of the owned and committed recorded specs
check: build
	$(SQLTEST) check $(OWNED_TESTS) $(COMMITTED_SPECS)

# Run the owned specs; every one must pass
run: build
	$(SQLTEST) run $(OWNED_TESTS) $(BACKEND)

# Alias for run
test: run

# Replay the recorded specs and write a JSON report for the scoreboard.
# tools/replay.py runs every file in its own sqltest process, so memory stays
# bounded and a crashing spec fails alone. Failing specs are expected while
# the frontend grows, so they do not fail the target; `make score` compares
# the report with baseline.txt. Syntax errors are checked first (reported on
# stderr).
run-specs: build
	@if [ -n "$(strip $(SPECS_DIRS))" ]; then \
		$(SQLTEST) check $(SPECS_DIRS) > /dev/null && \
		python3 tools/replay.py $(SPECS_DIRS) $(REPLAY_LOCAL) --output $(SPECS_REPORT) $(REPLAY_OPTIONS); \
	else \
		echo "no recorded specs in $(SPECS)/"; \
	fi

# Compare the last replay with baseline.txt; fails on regressions. Specs
# under specs/local/ are never scored against baseline.txt.
score:
	python3 tools/score.py $(SPECS_REPORT)

# Bucket the last replay's failures by the feature that blocks them and
# rewrite FAILURES.md (the ranked list of what to implement next)
triage:
	python3 tools/triage.py $(SPECS_REPORT) --markdown FAILURES.md

# Rebuild specs/local/<source>/ for sources that commit only MySQL's results
# (GPL or license-unclear test suites): fetch the pinned source, re-run its
# importer and join the committed results/<source>/ by id and SHA-256.
materialize-local:
	python3 -I tools/materialize.py $(LOCAL_SOURCES)

# Replay the materialized local specs the same way as run-specs
run-local-specs: build
	@if [ -d $(LOCAL_SPECS) ]; then \
		$(SQLTEST) check $(LOCAL_SPECS) > /dev/null && \
		python3 tools/replay.py $(LOCAL_SPECS) --include-local --output $(LOCAL_SPECS_REPORT) $(REPLAY_OPTIONS); \
	else \
		echo "no local specs in $(LOCAL_SPECS)/; run make materialize-local"; \
	fi

# Compare the last local replay with baseline-local.txt
score-local:
	python3 tools/score.py --local $(LOCAL_SPECS_REPORT)

# Run a single test file: make run-one FILE=my-sqltests/basics.sqltest
run-one: build
	$(SQLTEST) run $(FILE) $(BACKEND)

# Clean build artifacts
clean:
	$(CARGO) clean --manifest-path $(RUNNER_DIR)/Cargo.toml
