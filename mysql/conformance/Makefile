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
# Replay report of the recorded specs, read by the scoreboard
SPECS_REPORT ?= $(or $(CARGO_TARGET_DIR),$(ROOT_DIR)/target)/mysql-specs-report.json

.PHONY: all build check run test run-specs run-one clean

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

# Replay every recorded spec and write a JSON report for the scoreboard.
# Failing specs are expected while the frontend grows, so they do not fail
# the target; the scoreboard compares the report with baseline.txt. Syntax
# errors are checked first because they also make the runner exit nonzero.
run-specs: build
	@if [ -d $(SPECS) ]; then \
		$(SQLTEST) check $(SPECS) && \
		{ $(SQLTEST) run $(SPECS) $(BACKEND) --output json > $(SPECS_REPORT) || true; } && \
		echo "wrote $(SPECS_REPORT)"; \
	else \
		echo "no recorded specs in $(SPECS)/"; \
	fi

# Run a single test file: make run-one FILE=my-sqltests/basics.sqltest
run-one: build
	$(SQLTEST) run $(FILE) $(BACKEND)

# Clean build artifacts
clean:
	$(CARGO) clean --manifest-path $(RUNNER_DIR)/Cargo.toml
