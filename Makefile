CARGO ?= cargo
GO    ?= go

.PHONY: build release test test-integration clean

build:
	$(CARGO) build

release:
	$(CARGO) build --release

test:
	$(CARGO) test --workspace

# Run integration tests (requires root, builds release first).
# Includes verification tests (~1s) and xfstests regression (~90s).
# First run will install xfstests dependencies via tests/scripts/setup_xfstests.sh.
test-integration: release
	cd tests/integration && \
		sudo env "PATH=$(PATH)" "HOME=$(HOME)" \
		"GOMODCACHE=$$($(GO) env GOMODCACHE)" \
		EROFS_RUN_XFSTESTS=1 \
		$(GO) test -v -timeout 600s ./...

clean:
	$(CARGO) clean
