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

```sh
cargo build --locked --release
install -m 0755 target/release/tmux-agent ~/.local/bin/tmux-agent
```

Restart a running daemon after replacing the binary:

```sh
tmux-agent daemon restart
```

## Verify

```sh
tmux-agent --version
tmux-agent doctor
tmux-agent daemon status
```

`doctor --json` produces a privacy-limited diagnostic report suitable for a
bug report after review. It does not include terminal transcripts or captured
pane contents.

## Update and rollback

`prefix + U` remains TPM's checkout-update operation. Updating the checkout may
raise its compatibility floor and bootstrap the pinned version only when the
current managed binary no longer satisfies that floor.

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
lock and refuses an invalid existing `manager` link rather than guessing.

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
