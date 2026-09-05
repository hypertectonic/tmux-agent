# Contributing

`tmux-agent` is an early project. Feedback, reproducible bug reports, detector
fixtures, documentation improvements, and focused code contributions are
welcome.

Real-world testing has focused primarily on Codex. Reports from Claude,
OpenCode, Grok, OMP, and Pi workflows are especially useful.

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

### SSH and Mosh UI gate

On Linux, run the mandatory CI transport gate with:

```sh
tests/transport-ui/run
```

It requires Rust/Cargo, a running Docker daemon accessible without sudo, tmux,
Mosh (`mosh-client` and `mosh-server`), `lsof`, Python 3, and GNU `timeout`.
The fixture image explicitly installs OpenSSH, tmux, Mosh and process-inspection
tools. Missing prerequisites fail before building or running tests. Ordinary
`cargo test` remains a separate fast-development path and may skip real Mosh
tests when tools are absent; it does not replace this gate.

The gate runs the production scanner and persistent UI with two real PTY
clients through SSH and Mosh. It checks a hidden, non-first remote pane,
initiating-client selection, spectator preservation, reconnect with stale
binding, rejected control without acknowledgement, and old-peer partial focus.
The old-peer adapter removes only the remote-focus capability from production
watch output; it does not invent agent records. Named existing Rust tests also
check stale lifetime/target identities, client replacement, and missing-target
acknowledgement. Each case prints a result, and a missing or skipped named test
fails the gate.

Root-owned sshd and the unprivileged scanner run inside a disposable container
with no external network or published ports. Keys, login configuration, tmux
sockets, daemons and UI clients are fixture-only. The test verifies that the
scanner user cannot read its SSH login process's descriptor table. It never
uses a developer's default tmux server, hosts or credentials. Container and
process waits are bounded, and cleanup runs on failure.

For regression diagnosis, `tests/transport-ui/run --binary /absolute/path` runs
the container UI cases against an existing native Linux binary. This mode does
not build source or run the named Rust tests and is not the full CI gate. Both
modes mount the current fixture files, including local untracked edits.

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
