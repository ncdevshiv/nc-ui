# NC-UI local quality gates.
#
# `make gate` runs exactly what CI runs. Every target is a hard gate: a non-zero exit is a
# failure, not a warning. On platforms without `make`, run the commands listed beside each
# target (they are identical to the CI steps).

.PHONY: gate fmt lint test doc tools policy bench clean help

## gate: run every gate in the order CI does
gate: fmt lint test doc tools policy
	@echo ""
	@echo "all gates passed"

## fmt: formatting must be clean
fmt:
	cargo fmt --all -- --check

## lint: clippy at pedantic with warnings denied
lint:
	cargo clippy --workspace --all-targets -- -D warnings

## test: unit, integration and doc tests
test:
	cargo test --workspace --all-targets

## doc: documentation must build without warnings
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

## tools: the verification tooling must itself be clean
tools:
	cd tools && cargo fmt --all -- --check
	cd tools && cargo clippy --all-targets -- -D warnings
	cd tools && cargo test --all-targets

## policy: the project's own rules, as executable checks
# The scanner is invoked once per workspace root. Exemption is decided by a file's first
# path component, so scanning `tools` as a subdirectory of the repository would make every
# test file look like production source. Each workspace is its own root.
policy:
	cd tools && cargo build --release
	./tools/target/release/ncui-placeholders crates protocols
	cd tools && ../tools/target/release/ncui-placeholders ncui-tools
	./tools/target/release/ncui-arch .
	./tools/target/release/ncui-registry protocols/feature-registry.json

## bench: run the benchmark suite and report percentiles
bench:
	cargo bench --workspace

## clean: remove build output
clean:
	cargo clean
	cd tools && cargo clean

## help: list the available targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## /  /'
