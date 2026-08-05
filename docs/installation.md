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
the tmux-agent data directory. Use `--no-restart` when no running daemon should
be restarted.

The stable launcher is `bin/tmux-agent`. Add it to `PATH` or invoke it by its
absolute path.

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
tmux-agent plugin update # deprecated compatibility repair
tmux-agent plugin versions
tmux-agent plugin rollback <version>
```

`tmux-agent plugin update` is retained as a deprecated migration command. It
checks the current checkout's compatibility floor and repairs an absent,
below-minimum, or protocol-incompatible managed installation; it does not seek
a latest release and never replaces a newer compatible binary with the
checkout-pinned version.

Rollback selects an already installed version compatible with the current
checkout and restarts the daemon. Bootstrap and rollback serialize through the
same installation lock and switch the `current` symlink only by atomic rename.

## Uninstall

Remove the TPM plugin line, then press `prefix + alt + u`, or run:

```sh
scripts/uninstall
```

Use `scripts/uninstall --purge` only when runtime state, acknowledgements, and
all managed versions should also be removed.

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
