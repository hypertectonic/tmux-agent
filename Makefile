.PHONY: build test shell-test fresh-install-test lint audit validate install

build:
	cargo build

test:
	cargo test --locked

shell-test:
	tests/run-shell-tests

fresh-install-test:
	tests/fresh-install/run

lint:
	cargo fmt --all --check
	cargo clippy --locked --all-targets --all-features -- -D warnings

audit:
	cargo audit

validate: lint test shell-test
	scripts/check-version
	scripts/check-public-tree
	scripts/check-third-party-licenses
	scripts/check-release-readiness

install:
	cargo install --path .
