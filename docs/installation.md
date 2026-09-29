# Installation

tmux-agent supports macOS and Linux with tmux 3.2 or newer. Release installs
need `curl` or `wget`, `tar`, and a SHA-256 tool. Building from source also
requires the Rust toolchain.

## TPM installation

Add the plugin before the TPM `run` line in `~/.tmux.conf`:

```tmux
set -g @plugin 'hypertectonic/tmux-agent'
set -g @tmux-agent-key 'A'
```

Press `prefix + I` to install plugins. Press `prefix + A` to open tmux-agent.

The checkout's `COMPATIBILITY` file declares the launcher protocol and minimum
managed-binary version it requires. If the current managed binary has that
protocol and is at or above the minimum, the launcher keeps it, including when
it is newer than the version in the checkout. Otherwise the launcher downloads
the version recorded in `VERSION`, verifies `SHA256SUMS` and the reported
binary version, publishes an immutable version directory, and atomically makes
that binary current. It does not download an unpinned latest release.

### TPM options

```tmux
# Change the popup key.
set -g @tmux-agent-key 'A'

# Change popup dimensions.
set -g @tmux-agent-popup-width '80%'
set -g @tmux-agent-popup-height '80%'

# Use an existing binary instead of managed release installation.
set -g @tmux-agent-binary '/absolute/path/to/tmux-agent'
```

Existing key bindings are preserved. If the requested key is already bound,
tmux-agent reports the conflict and does not replace it.

## Standalone release installation

Clone the repository and run:

```sh
scripts/install
```

The installer selects the native archive, verifies it, and installs it under
the tmux-agent data directory. It atomically installs a checkout-independent
launcher at `~/.local/bin/tmux-agent`; set `TMUX_AGENT_INSTALL_PATH` to choose
another exact launcher path. Use `--no-restart` when no running daemon should
be restarted.

If that launcher path already contains a direct standalone tmux-agent binary,
the installer copies and verifies it under `versions/<version>` while holding
the shared installation lock. The original version remains an available
recovery target. The direct path is replaced with the stable launcher only
after a compatible managed binary is active. A failed migration leaves the
direct binary untouched. When the direct binary and checkout have the same
version, the binary must be byte-identical to the checksum-verified release;
a custom same-version build is left untouched and the installation fails
closed. Symlinked store collisions, launcher-path symlinks, and unrelated files
are refused rather than overwritten. An older official launcher is recognized
by its exact versioned format header and upgraded atomically without being
misclassified as a direct binary.

## Build from source

The following installs at the normal command path. For temporary testing, use
the separate-artifact and activation requirements below instead of overwriting
an existing official launcher.

```sh
cargo build --locked --release
install -m 0755 target/release/tmux-agent ~/.local/bin/tmux-agent
```

Restart a running daemon after replacing the binary:

```sh
tmux-agent daemon restart
```

### Experimental activation and return

These are required outcomes for temporary builds and their activation, not a
built-in deployment command. Preparation alone must not install, select, or
restart anything. Obtain approval for the exact hosts, runtimes, and live
process changes before activation; publication and releases are separate acts.

#### Prepare traceable artifacts

- Record the full source commit and build inputs. Prefer a clean source snapshot;
  if testing uncommitted work, preserve and identify its exact patch too. A
  commit SHA alone must not stand for different source bytes.
- Give the experiment a distinguishable version tied to that source. Keep
  temporary version stamping in a disposable build copy and record it; do not
  create empty commits or rewrite source history just to obtain a build ID.
- Build natively for each target or use an artifact with verified OS,
  architecture, and runtime-library compatibility. Record its checksum and
  embedded version. A successful build on one platform proves nothing about
  another platform's artifact.
- Keep experimental artifacts separate from official launchers and managed
  versions. Preserve the previous binaries, configuration, selector values
  including unset values, and launch commands needed to undo activation.

#### Select the whole runtime

Identify the user, configuration, environment, exact tmux socket, server
lifetime, and corresponding daemon IPC socket on each target. Verify the
resolved sockets, not just a session name or the invoking shell's `TMUX` value;
`tmux_args` and runtime-directory settings can change the destination.

Current selection paths are independent:

- `@tmux-agent-binary` selects plugin startup/popup behavior. It does not
  redirect an arbitrary shell command or replace an already-running UI.
- The checkout and standalone launchers select managed binaries. Runtime
  `current` and lifecycle `manager` have distinct purposes; the checkout
  launcher's `TMUX_AGENT_BINARY` override is not a universal selector.
- `[[machine]].binary` constructs remote `watch --jsonl --local-only`,
  `remote-focus`, `remote-handoff`, `subagent-view --local-only`, and diagnostic commands.
  Raw `[[remote]].command` collectors have their own command vectors.
- Commands that ensure a daemon exists can start their own executable when
  its socket is absent. Local child viewers also launch from the UI's own
  executable. Existing owned PTY runners retain their loaded code.

Before replacing the daemon, reconcile these entry points for the intended
experiment, including collectors launched by other participating hosts. Replace
or stop/reconnect only verified app-owned stale clients that could restart the
wrong daemon. Restart the selected daemon and affected UI/watch processes with
the agreed configuration. Include marked `@tmux_agent_ui=1` panes, hidden UIs,
popups, and any active child viewer or owned PTY runner relevant to the feature.
Do not respawn provider sessions just to refresh runner code; schedule such
changes with the user or mark that part unverified.

Scope cleanup by user, PID/start time, loaded executable, and runtime ownership.
Do not use name-wide kills, kill a tmux server, or terminate unrelated agent,
SSH, or Mosh sessions. Preserve layouts and user work.

#### Verify before reporting success

- After a settle period and collector reconnection, identify each relevant
  process's PID/start time and loaded executable using platform process
  inspection. Correlate the loaded image with the recorded artifact/version;
  checking only `--version` on a replaced pathname or `pane_current_command`
  does not identify code already running. Check daemon IPC ownership and every
  affected UI pane, recording expected, restarted, and verified counts.
- Verify each command through its actual launch route and environment,
  including remote non-interactive SSH. Observe the executable/version used
  by short-lived `remote-focus`, `remote-handoff`, diagnostics, and child-view commands during
  an isolated exercise, not merely a different shell's `command -v`. Account
  for the tmux/SSH/Mosh helpers and sockets those commands actually use.
- Check the local-only daemon snapshot and the consuming host's merged
  snapshot. For remote tmux focus, verify required `session_connections`
  identity survives the actual watcher. A connected peer, matching protocol,
  capability, or advertised daemon version does not prove relay compatibility:
  `watch` decodes and reserializes a typed snapshot, so an older executable can
  drop newer fields while forwarding those labels.
- Exercise the feature end to end. Remote focus must confirm the requested
  inner session/window/pane IDs and the initiating local client's destination.
  CLI success or outer-only focus is not evidence of inner selection. Use
  isolated fixtures such as `tests/transport-ui/run` by default; moving a real
  user's focus requires explicit approval. Do not open private transcripts to
  test child viewers when a synthetic fixture suffices.

Record source/version, per-target artifact paths and hashes, sockets, verified
processes and commands, UI counts, feature results, and rollback selection.
If tooling reports success but any required check fails or cannot be completed,
report activation incomplete and investigate. Do not substitute daemon-only
health for feature verification.

#### Return to official builds

Restore the recorded official launch routes and configuration, removing only
the experiment's overrides. Reconcile remote collector/control/viewer commands
as well as local daemon and UI selection. Stop/reconnect verified experimental
watchers before they can restart an experimental daemon. Restart affected
app-owned processes and repeat the same loaded-image, socket, snapshot, UI, and
feature checks against the intended official version. A changed symlink or
`rollback` exit code alone is not proof. Retain recovery artifacts until the
return is verified; deletion requires separate authorization.

## Verify

```sh
tmux-agent --version
tmux-agent doctor
tmux-agent daemon status
```

`doctor --json` produces a privacy-limited diagnostic report suitable for a
bug report after review. It does not include terminal transcripts or captured
pane contents.

## Agent handoff skill

The handoff commands require tmux-agent 0.10.0 or newer on the sender and
recipient. Update each participating machine and check
`tmux-agent handoff --help`. Installing a skill does not upgrade the binary.

The optional [skill](../skills/tmux-agent-handoff/SKILL.md) works with any
harness that can read instructions and run shell commands:

1. Take `skills/tmux-agent-handoff/` from the same source revision as the binary.
2. Copy or symlink that directory into your harness's supported skill location
   on each machine where an agent will send handoffs. Inspect an existing copy
   before replacing it. Do not link to a disposable worktree you plan to remove.
3. Reload skills or start a new agent session as your harness requires. Confirm
   that it can read `SKILL.md` and run `tmux-agent find --help`.

Without automatic skill discovery, explicitly ask the agent to read the
installed `SKILL.md` before using handoff. The recipient can receive ordinary
prompt text without installing a skill; loading it also explains how to handle
peer requests and report the handoff ID. No additional daemon or plugin is needed.

Release binary installation does not install the skill. A sandboxed sender
must be able to read its resolved skill path, reach the local daemon socket and
handoff state directory shown by `tmux-agent paths`, and use configured SSH for
remote sends. Pi's normal sandbox blocked those paths in private testing;
receiving still worked. Follow your harness's permission mechanism rather
than disabling sandboxing or adding a raw tmux fallback automatically.

When testing a private build, preserve the previous binary and selections.
Check the shell command, TPM `@tmux-agent-binary` option and every machine's
`binary`/`config` paths; they must select the intended build and tmux server.
Restart the affected daemons, reconnect their collectors and restart existing
UI processes explicitly. Verify versions, peer capabilities and the actual
running processes. A binary replacement alone does not reload them. Keep these
private selections separate from managed release update/rollback, and restore
the recorded selections when returning to an official build.

See [Agent handoff](handoff.md) for a first send and its delivery limits.

## Agent workspace skill

The optional [workspace skill](../skills/tmux-agent-workspace/SKILL.md) creates
ordinary tmux task windows or explicitly requested sessions, launches an
interactive worker, then uses handoff for its assignment. It is agent guidance,
not a new `tmux-agent` subcommand. It is independent of the initiating
harness and any personal dotfiles or worktree manager.

Install `skills/tmux-agent-workspace/` alongside `skills/tmux-agent-handoff/`
using the copy/symlink and reload procedure above. Keep both from the same source
revision and make their instructions readable to the initiating agent. Without
skill discovery, ask it to read both `SKILL.md` files explicitly. Installing or
updating the binary does not install these skills.

The initiating agent needs shell access to the destination machine and tmux
server, plus the existing handoff prerequisites. The worker command defaults
to `codex` in the destination environment. Supply another command, such as
`codex2` or `claude`, an executable path, or a `CODEX_HOME` override explicitly
when needed. The skill does not install providers, configure authentication,
bypass permissions, or create a separate home directory for a fresh conversation.

Task monitoring uses metadata and, when needed, reads the worker's output on
its owning machine. That output may include private task details; it is not
part of federation snapshots. Monitoring belongs to the initiating agent, not
to a new daemon, and does not survive that agent stopping.

## Update and rollback

`prefix + U` remains TPM's checkout-update operation. Updating the checkout may
raise its compatibility floor and bootstrap the pinned version only when the
current managed binary no longer satisfies that floor.

Use the same checkout entrypoint for an initial TPM install and a checkout
upgrade. Use `scripts/install` for an initial standalone install or to migrate
a pre-launcher direct binary. Once either mode has a managed launcher, use the
packaged lifecycle commands below for binary releases; reinstalling a checkout
is not the normal binary-update path.

With the stable launcher:

```sh
tmux-agent update
tmux-agent update --version <version>
tmux-agent versions
tmux-agent rollback <version>
tmux-agent plugin update # deprecated; behavior depends on launcher type
tmux-agent plugin versions # legacy alias
tmux-agent plugin rollback <version> # legacy alias
```

`tmux-agent update` is the packaged-binary update path. It needs no Git
checkout, Rust toolchain, GitHub CLI, browser, or GitHub credentials. By
default it reads the latest stable release metadata from the canonical public
GitHub repository over HTTPS. It validates the release tag, constructs
immutable version-pinned archive and checksum URLs, verifies the native target,
checksum, archive allowlist, compatibility metadata, and embedded binary
version, then stages the release in the managed version store and atomically
activates it. Implicit curl/wget configuration and netrc credentials are
disabled, and downloads, extraction, and binary probes are bounded. A
prerelease is accepted only when its exact semantic version is provided with
`--version`.

The daemon is restarted only after activation succeeds. If discovery,
download, verification, staging, activation, or restart fails, the previous
`current` binary remains usable; a restart failure also restores the previous
activation. Re-running at the current version is a no-op, and a discovered or
requested older version never replaces a newer current binary. When `--config`
is supplied, the same configuration is used for both the updated daemon restart
and any rollback restart.

`tmux-agent plugin update` is retained as a deprecated migration command. Under
the TPM checkout launcher it checks the checkout's compatibility floor and
repairs an absent, below-minimum, or protocol-incompatible managed installation;
it does not seek a latest release and never replaces a newer compatible binary
with the checkout-pinned version. The checkout-independent standalone launcher
has no checkout to repair, so its exact `plugin update` alias delegates to
packaged `tmux-agent update`. The legacy aliases enforce their documented
argument counts.

`tmux-agent versions` identifies the active version and lists the other
verified native versions as available rollback targets. `tmux-agent rollback`
revalidates the selected directory, metadata, platform, and embedded binary
version while holding the same installation lock used by bootstrap and update.
It switches the `current` symlink only by atomic rename and restarts the daemon.
Activation or restart failure restores the previously active version.
The stable launcher routes only `update`, `versions`, and `rollback` (including
forms preceded by global `--config`) through the separately verified `manager`
selection. All normal commands continue through `current`. Rollback never moves
`manager`, so lifecycle commands remain available even when the selected
runtime predates those commands. Update and bootstrap also keep a verified
`manager` whose version is newer than the candidate, preventing controller
downgrades while allowing `current` to move independently.

The legacy `plugin versions` and `plugin rollback` forms delegate to these
packaged commands; the TPM checkout no longer owns a separate rollback
implementation. TPM's `prefix + U` updates only the plugin checkout; it does
not replace `tmux-agent update` or select a packaged binary release. Existing
TPM-managed stores are reused without moving `current` back under checkout
control. Bootstrap installs or repairs a missing controller under the shared
lock and refuses an invalid existing `manager` link rather than guessing. A
pre-self-update TPM runtime that has the exact in-store legacy layout, meets the
checkout compatibility floor, and has no `manager` is preserved as a rollback
target by adding only its native target and launcher metadata. Partial,
symlinked, out-of-store, same-version, or otherwise ambiguous legacy layouts
are not migrated.

The release-candidate gate tests fresh standalone and TPM stores separately.
It also tests a direct standalone upgrade and the exact metadata-absent TPM
store produced by the public v0.3.0 release against a genuinely newer
candidate. The gate pins the four v0.3.0 archive checksums and rejects a
same-version candidate instead of presenting reinstall coverage as a
cross-version upgrade.

## Uninstall

Remove the TPM plugin line, then press `prefix + alt + u`, or run:

```sh
scripts/uninstall
```

Use `scripts/uninstall --purge` only when runtime state, acknowledgements, and
all managed versions should also be removed.

The uninstaller also removes the standalone launcher at
`TMUX_AGENT_INSTALL_PATH` (default `~/.local/bin/tmux-agent`) only when it is a
regular executable with the exact managed-launcher format header. Unrelated
executables and symlinks at that path are reported and retained.

## Install with a coding agent

Give the coding agent this file and the repository URL. It should:

1. Detect whether TPM is installed.
2. Add the plugin line only when absent, or run `scripts/install`.
3. Preserve existing key bindings, windows, panes, and layouts.
4. Reload the existing tmux configuration safely.
5. Run `tmux-agent doctor --json`.
6. Report the version, key binding, daemon status, and any remaining manual
   action.

It must not install provider aliases, alter provider commands, or configure an
SSH machine without exact host information from the user.
