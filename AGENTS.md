# Working on tmux-agent

Read [README.md](README.md) for supported behavior and
[CONTRIBUTING.md](CONTRIBUTING.md) for development prerequisites and checks.
`CLAUDE.md` is a relative symlink to this file; keep instructions here only.

## Where to look

- CLI commands: `src/main.rs`. Configuration, SSH command construction, and
  runtime paths: `src/config.rs`.
- Missing agents or wrong activity: `src/scanner.rs`, `src/detect.rs`,
  `src/detect/provider/`, and `src/detect/stabilize.rs`. Synthetic screen
  fixtures live in `tests/fixtures/detection/`; Rust tests live beside the code.
- Process and tmux identity: `src/tmux.rs` and `src/tmux/linux_ssh.rs`.
  Focus orchestration and remote inner selection: `src/focus.rs`.
  Real transport/UI scenarios: `tests/transport-ui/`.
- Snapshots, federation, and persistence: `src/model.rs`, `src/daemon.rs`,
  `src/ipc.rs`, and `src/store.rs`. UI input, rendering, and activation:
  `src/ui.rs`.
- Owned PTY sessions: `src/runner.rs`. Codex metadata and child ownership:
  `src/codex.rs` and `src/codex/`. Explicit child viewing: `src/transcript.rs`.
- Agent discovery filters and delivery: `src/handoff.rs` and `src/handoff/`.
  Harness-neutral handoff and workspace guidance lives in `skills/`.
- Installation and lifecycle: `src/update.rs`, `tmux-agent.tmux`,
  `bin/tmux-agent`, and `scripts/`. Shell integration tests are in `tests/*.sh`.
  `.github/workflows/` defines CI and release gates.
- Read [architecture](docs/architecture.md) for contracts,
  [remote machines](docs/remote-machines.md) for federation/focus,
  [installation](docs/installation.md) and
  [troubleshooting](docs/troubleshooting.md) for runtime work, and the
  [release checklist](docs/release-checklist.md) for release-only gates.

## How to work

- Start focused branches from current `develop`; target PRs at `develop`.
  `main` is released history. Follow existing conventional commit titles and
  preserve implementation history when integrating.
- Inspect status and existing instructions first. Preserve unrelated edits and
  worktrees. Use an isolated branch/worktree for independent work; respect an
  existing allocator's ownership and leases when present.
- Prefer the simplest focused change. Reproduce bugs before fixing them and
  test meaningful behavior at existing public seams. Keep documentation,
  examples, comments, and fixtures aligned with changed behavior.
- Keep captured terminal content on its owning machine and out of normal
  federation snapshots. Use minimal synthetic fixtures, not private prompts,
  transcripts, command arguments, hostnames, paths, or process metadata.
  Review diagnostic output before sharing it; see [SECURITY.md](SECURITY.md).
- Code preparation does not authorize publishing, merging, releasing, or
  activating a build. Respect the user's target and action scope. Report
  validation failures and unverified behavior instead of bypassing gates.

## Validation

For code changes, run the relevant focused tests, then the applicable checks:

```sh
cargo build --locked
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
tests/run-shell-tests
scripts/check-version
scripts/check-public-tree
```

For docs-only changes, check links and command accuracy, run
`tests/documentation-smoke.sh`, `scripts/check-public-tree`, and
`git diff --check`. Run `scripts/check-third-party-licenses` for dependency
changes and regenerate the report when `Cargo.lock` changes. Follow
CONTRIBUTING's SSH/Mosh gate and the release checklist when those areas apply;
state unavailable prerequisites and skipped checks explicitly.

## Experimental builds and activation

Follow the required outcomes in
[experimental activation and return](docs/installation.md#experimental-activation-and-return).
Record exact source and artifact identity, select the target runtime/socket
explicitly, preserve the previous selection, and verify every participating
daemon, UI, watcher, and helper's actual executable/version. A disk binary or
green protocol label is insufficient. Use suitable available tooling, but do
not treat its success message as proof. The repository currently has separate
selection paths, not a universal activation switch.

Verify remote focus reaches the requested inner window and pane, not only the
outer transport. Use isolated fixtures for focus-changing tests unless the
user explicitly approves a live test. Never kill unrelated tmux or agent
sessions. Verify the same command/process chain when returning to official
builds; report activation incomplete if any required check fails or is missing.
