# NC-UI local quality gates.
#
# `make gate` runs exactly what CI runs. Every target is a hard gate: a non-zero exit is a
# failure, not a warning. On platforms without `make`, run the commands listed beside each
# target (they are identical to the CI steps).

.PHONY: gate fmt lint test doc tools policy client bench clean help

## gate: run every gate in the order CI does
gate: fmt lint test doc tools policy client
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
	./tools/target/release/ncui-placeholders fuzz
	./tools/target/release/ncui-arch .
	./tools/target/release/ncui-registry protocols/feature-registry.json
	python tools/correct_test_counts.py --check
	./tools/target/release/ncui-placeholders packages
	./tools/target/release/ncui-placeholders examples
	./tools/target/release/ncui-placeholders comparison/ports comparison/bench/instrumentation
	python tools/make_greeter_component.py --check
	python comparison/tools/make_word_signal_component.py --check

## component-example: regenerate the example's WebAssembly artifact
# The artifact ships as WAT text and is generated, so the numbers in it are derived rather
# than typed. Regenerating it must not change a byte; `make policy` runs the check, because a
# generator whose output drifted from the file in the tree is a generator nobody can trust.
component-example:
	python tools/make_greeter_component.py

## bundle: build a shippable application directory
# The runtime must be built first: a bundle is the binary plus the application, and `ncui build`
# refuses to invent one. The second line is the check — it compares the bundle in `dist` with a
# fresh build of the tree, which is how a stale bundle is caught rather than shipped.
bundle:
	cargo build --release -p ncui-cli --features ncui-cli/wry-host
	./target/release/ncui build examples/counter
	./target/release/ncui build examples/counter --check

## client: the page-side package must pass its own suite
# The bridge is the page's half of the runtime's contract, and the host injects the very
# file this package tests. The suite runs the injected script, not a copy of it.
#
# The files are named rather than the directory given: `node --test <directory>` resolves the
# directory as a module on some Node versions and scans it on others, so the same command passes on
# one machine and reports `MODULE_NOT_FOUND` on the next. A glob keeps the property that matters —
# a new `*.test.js` is picked up without editing this file — and behaves the same everywhere.
client:
	node --test packages/ncui-client/tests/*.test.js

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

# --- Tier 1 guards (additive appendix; existing targets above unchanged) ---
# Mirrors the `policy` job steps in .github/workflows/ci.yml. On platforms
# without `make`, run the commands beside each target (identical to CI).
.PHONY: file-size changelog-check changelog-fold hygiene

## file-size: file-size + folder-shape (Tier 1)
# Windows: python scripts/file-size.py  (bash: bash scripts/file-size.sh)
file-size:
	python3 scripts/file-size.py

## changelog-check: changelog fragments are well-formed (Tier 1)
# Windows: python scripts/changelog-fold.py --check
changelog-check:
	python3 scripts/changelog-fold.py --check

## changelog-fold: fold changelog/unreleased/*.md into CHANGELOG.md (release-time only)
# Never run in a feature PR; the maintainer folds at release.
changelog-fold:
	python3 scripts/changelog-fold.py --check

## hygiene: file-size + folder-shape + secret/absolute-path scan (Tier 1)
# Windows parity: pwsh -File scripts/hygiene-scan.ps1 --tree
# bash parity: bash scripts/hygiene-scan.sh --tree
hygiene:
	python3 scripts/file-size.py
	python3 scripts/hygiene-scan.py --tree
