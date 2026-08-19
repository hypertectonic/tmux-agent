# Troubleshooting

Start with:

```sh
tmux-agent doctor
tmux-agent daemon status
tmux-agent list
```

Use `tmux-agent doctor --json` for a bug report after reviewing the output. Do
not attach terminal transcripts, pane captures, credentials, or private SSH
configuration.

## The popup does not open

Check the configured binding:

```tmux
tmux list-keys -T prefix
```

tmux-agent preserves an existing binding instead of replacing it. Choose a
different `@tmux-agent-key`, reload the tmux configuration, and try again.

Confirm that TPM sourced `tmux-agent.tmux` and that `bin/tmux-agent` is
executable.

## A provider session is missing

- Confirm the provider is Codex, Claude, OpenCode, Grok, OMP, or Pi.
- Run `tmux-agent scan --json` on the machine that owns the session.
- Confirm the agent process is in the foreground process group.
- Inside tmux, make the pane visible once so the detector can inspect its
  current terminal surface.
- Outside tmux, use an owned PTY shortcut for screen-based state detection.

## A session shows unknown

Process-only discovery proves that a supported provider exists but cannot
always prove its current activity. Run the provider inside tmux or through
`tmux-agent codex`, `tmux-agent claude`, `tmux-agent opencode`,
`tmux-agent omp`, `tmux-agent pi`, or the generic
`tmux-agent run -- <command>` wrapper.

## The daemon uses an old version

```sh
tmux-agent update
tmux-agent daemon restart
tmux-agent doctor
```

If update fails, the prior managed binary remains active. Inspect the reported
verification or network error, then re-run `tmux-agent update`; completed
versions are immutable and re-running the current version is a no-op.

If the plugin launcher and direct shell command resolve different binaries,
use `tmux-agent paths` and `command -v tmux-agent` to compare them.

## An update was interrupted or failed verification

Discovery, download, checksum, archive, target, compatibility, and embedded
version failures leave the prior `current` selection active. Do not move files
inside the managed store manually. Correct the network or release-input error,
then run:

```sh
tmux-agent update
tmux-agent versions
```

Temporary `.update-*` and `.staging-*` directories are cleaned on normal error
paths. A completed immutable version directory is revalidated and reused. If a
daemon restart failed, update restores the prior activation and attempts to
restart that binary; use `tmux-agent daemon status` and `tmux-agent doctor`
before retrying.

## A lifecycle command is waiting for the installation lock

Bootstrap, update, version listing, and rollback serialize on
`~/.local/share/tmux-agent/.install.lock` (or the configured data directory).
Let the other operation finish and retry. A lock owned by a dead process is
recovered automatically; an incomplete lock is given a grace period so a
concurrent process publishing its owner is not mistaken for stale state. Do
not remove a lock held by a live process.

## A pre-self-update TPM installation does not migrate

The automatic migration recognizes only the exact v0.3-style layout: `current`
must select a real executable under `versions/<version>`, both `TARGET` and
`COMPATIBILITY` must be absent (or the exact resumable `TARGET` state may be
present), and `manager` must be absent. The binary must be older than the new
checkout and meet its compatibility floor.

Partial metadata, an existing invalid manager, symlinks inside the version
directory, an out-of-store selection, and ambiguous same-version binaries fail
closed. Preserve the reported state and install from a current trusted
checkout; do not add metadata or rewrite `current` by hand.

## A remote peer is disconnected

Verify SSH independently:

```sh
ssh -o BatchMode=yes agent@build-host.example.ts.net \
  '/home/agent/.local/bin/tmux-agent --version'
```

Then confirm the configured binary path, SSH user, host key, and application
protocol version. Restart the central daemon after correcting configuration.

## Remote focus fails

Remote focus requires a unique local tmux pane carrying the matching SSH
connection or a mirror pane with the public remote marker options. Multiple
matching panes are rejected. A remote tmux agent is not matched to a local SSH
pane by title alone. Use `tmux-agent explain <id-or-pane>` and `tmux-agent
doctor` to inspect the derived target without exposing pane contents.

For nested remote tmux over mosh, bind the correct local transport pane
explicitly. Run the command on the local machine, not inside the remote tmux
session:

```sh
tmux-agent remote bind <configured-remote> <remote-tmux-session> --pane <local-pane-id>
```

Use `tmux-agent remote bindings` to inspect current mappings and `tmux-agent
remote unbind --pane <local-pane-id>` to remove one.

## A completion remains visible

Activate the row with `Enter` or a left click, or run:

```sh
tmux-agent acknowledge <id-or-pane>
```

Acknowledgement remains effective until that agent begins another active
turn.

## Roll back

```sh
tmux-agent versions
tmux-agent rollback <version>
```

Rollback uses an already installed, verified native version. It refuses a
missing, active, incompatible, symlinked, or corrupt target. If daemon restart
fails after activation, the previously active version is restored and
restarted. Check `tmux-agent daemon status`; if both restart attempts fail, the
reported activation still identifies which binary was restored, and the error
reports the second failure explicitly. Legacy `plugin versions` and `plugin
rollback` commands delegate to the same packaged implementation. If the
launcher reports that no verified lifecycle controller is available, run
`scripts/install` from a current trusted checkout; an existing malformed
`manager` link is rejected rather than silently replaced.
