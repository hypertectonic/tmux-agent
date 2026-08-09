# Contributing

`tmux-agent` is an early project. Feedback, reproducible bug reports, detector
fixtures, documentation improvements, and focused code contributions are
welcome.

Real-world testing has focused primarily on Codex. Reports from Claude,
OpenCode, Grok, and Pi workflows are especially useful.

## Before opening a change

For behavior changes, start with a concise description of the problem and the
terminal environment in which it occurs:

- Operating system and architecture.
- tmux version.
- Agent and agent version.
- Whether the agent runs locally, through SSH, or outside tmux.
- The derived state that appeared and the state that was expected.

Do not post credentials, private hostnames, Tailnet details, complete process
command lines, raw terminal transcripts, or captured pane contents.

## Development

Build and validate the project with:

```sh
cargo build --locked
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
tests/run-shell-tests
scripts/check-version
scripts/check-public-tree
scripts/check-third-party-licenses
```

Behavior changes to detection or state transitions should include focused
tests. Remote behavior must preserve SSH as the transport and must not add pane
contents to federation snapshots.

## Change scope

Keep changes focused and preserve backward-compatible snapshot formats unless a
protocol version is intentionally incremented. The tmux plugin layer should
remain a thin launcher. Detection, state transitions, focus resolution, and
federation belong in the Rust application.

Shell behavior changes should include isolated launcher, installer, or tmux
integration coverage. Regenerate `THIRD_PARTY_LICENSES.html` with
`scripts/generate-third-party-licenses` whenever `Cargo.lock` changes. Run
`scripts/check-release-readiness` and `cargo audit` before a release candidate.
The cross-version and supported-target release gates are defined in
[the release checklist](docs/release-checklist.md).
