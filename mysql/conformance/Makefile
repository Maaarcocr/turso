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
# Recorded specs to replay: every committed source, plus the ignored local
# recordings with INCLUDE_LOCAL=1.
SPECS_DIRS := $(filter-out $(SPECS)/local,$(patsubst %/,%,$(wildcard $(SPECS)/*/)))
ifdef INCLUDE_LOCAL
    SPECS_DIRS += $(wildcard $(SPECS)/local)
    REPLAY_LOCAL := --include-local
endif
# Replay report of the recorded specs, read by the scoreboard
SPECS_REPORT ?= $(TARGET_ROOT)/mysql-specs-report.json
# sqltest processes at a time, seconds per setup or test block, and the
# address-space limit of each process (see tools/replay.py)
SPECS_JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)
SPECS_TIMEOUT ?= 30
SPECS_MEMORY_MB ?= 2048

.PHONY: all build check run test run-specs score run-one clean

# Build everything
all: build

# Build the sqltest runner, which embeds the tursomysql backend
build:
	$(CARGO) build --manifest-path $(RUNNER_DIR)/Cargo.toml $(CARGO_PROFILE)

# Check syntax of the owned and recorded specs
check: build
	$(SQLTEST) check $(OWNED_TESTS) $(wildcard $(SPECS))

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
		python3 tools/replay.py $(SPECS_DIRS) $(REPLAY_LOCAL) --sqltest $(SQLTEST_BIN) \
			--output $(SPECS_REPORT) --jobs $(SPECS_JOBS) --timeout $(SPECS_TIMEOUT) \
			--memory-mb $(SPECS_MEMORY_MB); \
	else \
		echo "no recorded specs in $(SPECS)/"; \
	fi

# Compare the last replay with baseline.txt; fails on regressions
score:
	python3 tools/score.py $(SPECS_REPORT)

# Run a single test file: make run-one FILE=my-sqltests/basics.sqltest
run-one: build
	$(SQLTEST) run $(FILE) $(BACKEND)

# Clean build artifacts
clean:
	$(CARGO) clean --manifest-path $(RUNNER_DIR)/Cargo.toml
