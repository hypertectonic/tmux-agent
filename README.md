# tmux-agent

tmux-agent is a local-first activity view for coding agents running in tmux,
ordinary terminals, and explicitly configured SSH machines.

![tmux-agent showing local and remote agents, goals, and a child session](docs/assets/ui-overview.svg)

It shows which sessions are working, blocked, idle, or done and lets you jump
back to the exact tmux pane. Codex, Claude, OpenCode, Grok, and Pi have
project-owned typed detectors. Codex goal progress and nested subagents are
shown without forwarding the goal objective or rollout content in federation
snapshots.

> **Early release:** Real-world testing has focused primarily on Codex in local
> and SSH-based tmux workflows. Claude, OpenCode, Grok, and Pi are supported
> but have received less testing.

## Install with TPM

Requirements:

- tmux 3.2 or newer on macOS or Linux
- TPM
- `curl` or `wget`, `tar`, and a SHA-256 tool

Add the plugin before the TPM `run` line in `~/.tmux.conf`:

```tmux
set -g @plugin 'hypertectonic/tmux-agent'
set -g @tmux-agent-key 'A'
```

Press `prefix + I`, then `prefix + A`. When no compatible managed binary is
installed, the plugin downloads the version recorded in `VERSION`, verifies
the checksum and binary version, starts one daemon for the selected tmux
server, and opens an 80 percent popup. A newer compatible managed binary is
kept in place.

The plugin does not create a sidebar, split, window, or session. A pane where
you run `tmux-agent ui` can be used as a user-managed sidebar.

For a standalone installation, run `scripts/install`. It publishes a stable,
versioned launcher at `~/.local/bin/tmux-agent`, keeps runtime and lifecycle
controller selections independent, and safely upgrades older official
launchers. Direct binaries are preserved during migration; a same-version
binary must exactly match the verified release.

See [Installation](docs/installation.md) for standalone installation, plugin
options, update, rollback, and uninstall instructions.

## Update and rollback

The packaged binary owns its lifecycle in both installation modes:

```sh
tmux-agent update
tmux-agent versions
tmux-agent rollback <version>
```

`tmux-agent update` installs a newer verified release, `versions` shows the
active and available recovery versions, and `rollback` selects an already
installed version. Normal release updates are user-initiated: tmux-agent does
not poll for releases, update on a schedule, or issue lifecycle commands on an
SSH peer.

For TPM installations, `prefix + U` updates the plugin checkout rather than
seeking the latest binary release. Loading that checkout may run its narrow
compatibility repair: when `current` is absent, below the checkout's floor, or
protocol-incompatible, bootstrap verifies and activates the checkout-pinned
binary. Under the TPM launcher, deprecated `tmux-agent plugin update` runs that
same compatibility repair. Under the checkout-independent standalone launcher,
the legacy alias instead delegates to packaged `tmux-agent update`.

## Supported platforms

| Platform | Release target | Support |
| --- | --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` | Tier 1 |
| macOS Intel | `x86_64-apple-darwin` | Tier 1 |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | Tier 1 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Tier 2 |

Linux release archives require glibc 2.35 or newer. Windows is not supported.

## What it shows

- Codex, Claude, OpenCode, Grok, and Pi sessions in tmux and ordinary terminals.
- `working`, `needs input`, `idle`, `done`, and evidence-limited `unknown`
  states.
- Provider badges, working animation, task titles, pane labels, and host-first
  location breadcrumbs.
- Codex goal state and elapsed time without the goal objective.
- Process-backed and in-process Codex children beneath their actual parent.
- Local and SSH-federated machines in one view.

tmux-agent derives state from foreground process metadata and the visible
terminal surface. Ordinary terminal sessions are detected from their TTY but
remain `unknown` unless tmux-agent owns an inner PTY screen.

## Controls

Run the UI in a pane:

```sh
tmux-agent ui
```

| Input | Action |
| --- | --- |
| `j`, `k` | Move selection in normal mode; type those characters during search |
| Up, Down | Move selection in normal or search mode |
| `/` | Start filtering sessions as you type |
| `Backspace` | Edit the active search |
| `Enter` | Focus, acknowledge, or open a Codex child |
| Left click | Activate a row |
| `r` | Refresh |
| `q` | Close in normal mode; type `q` during search |
| `Esc` | Clear the active search, otherwise close |

Search is case-insensitive and matches the displayed title, provider, label,
state, location, and working directory. It remains local to the UI process.

A non-empty tmux pane label is appended to the task title:

```tmux
set -pt:. @pane_label 'linux integration'
```

For an SSH-federated agent, a label on the uniquely resolved local transport
pane is used by the local UI and takes precedence over a label reported by the
remote pane. Labels from ambiguous or unrelated panes are ignored.

## Running providers

Plain provider commands work inside tmux:

```sh
codex
claude
opencode
grok
pi
```

For screen-based state detection in an ordinary local or SSH terminal, use an
owned PTY shortcut:

```sh
tmux-agent codex [args...]
tmux-agent claude [args...]
tmux-agent opencode [args...]
tmux-agent run -- grok [args...]
tmux-agent pi [args...]
```

The wrapper forwards terminal input, output, resize events, signals, job
control, and the child exit status. It does not replace provider commands or
install shell aliases.

## Remote machines

Install the same tmux-agent version on every machine, then configure only the
machines you choose in `~/.config/tmux-agent/config.toml`:

```toml
[[machine]]
name = "build-host"
host = "build-host.example.ts.net"
ssh_user = "agent"
binary = "/home/agent/.local/bin/tmux-agent"
```

Restart and verify:

```sh
tmux-agent daemon restart
tmux-agent doctor
tmux-agent list
```

SSH supplies authentication, encryption, host-key policy, streaming, and the
separate interactive connection used for a remote Codex child view. Tailscale
can provide private reachability but is not an application dependency.

See [Remote machines](docs/remote-machines.md) for setup order, privacy
boundaries, focus behavior, and safe multi-machine updates.

## Privacy and security

- The daemon listens only on a mode `0600` local Unix socket.
- Runtime and state directories use mode `0700`.
- Captured terminal screens stay on the machine that owns them.
- Federation snapshots exclude captured pane contents, screen buffers, raw
  command lines, prompts, reasoning, rollout events, and goal objectives.
- Remote federation uses non-interactive SSH. There is no application TCP
  listener or shared application token.
- Codex rollout content crosses SSH only while the user has explicitly opened
  a read-only child view.

The child viewer can show tool output that contains sensitive paths, code, or
command results. Review that boundary before opening a remote child.

See [Security](SECURITY.md), [Third-party notices](THIRD_PARTY_NOTICES.md), and
the generated [dependency license report](THIRD_PARTY_LICENSES.html).

## Diagnostics

```sh
tmux-agent doctor
tmux-agent doctor --json
tmux-agent daemon start
tmux-agent daemon status
tmux-agent daemon restart
tmux-agent daemon stop
tmux-agent update [--version <version>]
tmux-agent versions
tmux-agent rollback <version>
tmux-agent paths
```

See [Troubleshooting](docs/troubleshooting.md) for missing sessions,
disconnected peers, focus failures, and rollback.

## Command reference

```text
tmux-agent scan [--json]
tmux-agent list [--json] [--local-only]
tmux-agent watch --jsonl [--local-only]
tmux-agent ui [--popup]
tmux-agent focus <id-or-pane>
tmux-agent explain <id-or-pane>
tmux-agent acknowledge <id-or-pane>
tmux-agent codex [args...]
tmux-agent claude [args...]
tmux-agent opencode [args...]
tmux-agent pi [args...]
tmux-agent run -- <command> [args...]
tmux-agent daemon start|status|restart|stop|run
tmux-agent doctor [--json]
tmux-agent update [--version <version>]
tmux-agent versions
tmux-agent rollback <version>
tmux-agent paths
```

## Install with a coding agent

Ask the coding agent to follow [Installation](docs/installation.md) and use
`https://github.com/hypertectonic/tmux-agent` as the repository.

The installer must preserve existing bindings and layouts, configure no SSH
machine without exact user-provided host information, and finish by running:

```sh
tmux-agent doctor --json
```

## Development

```sh
scripts/check-version
scripts/check-public-tree
scripts/check-release-readiness
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
tests/run-shell-tests
tests/fresh-install/run
```

The fresh-install test builds a context from tracked files only, creates real
release archives, and exercises standalone and tmux plugin installation as a
non-root user in clean ARM64 and AMD64 Linux containers. Docker is required
for this release-candidate test.

The [release checklist](docs/release-checklist.md) defines the cross-version
self-update gate, including the real v0.3.0 layout fixture and all four release
targets.

Architecture and protocol details are in
[Architecture](docs/architecture.md). Contributions are described in
[Contributing](CONTRIBUTING.md).

## License

tmux-agent is licensed under the [MIT License](LICENSE).

Dependency licenses and notices are provided in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html).
