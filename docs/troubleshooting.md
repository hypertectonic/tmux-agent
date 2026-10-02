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

First separate federation health from focus. A healthy `peer:<name>` check
means the collector is connected with a compatible protocol. It does not mean
an interactive SSH or Mosh attachment exists or that inner selection succeeded.
`focus:<name>` reports how many remote records have a cached local transport
binding in the daemon snapshot. Doctor does not select or verify an inner pane.

Current remote tmux records use `session_connections`, not the agent's inherited
`ssh_connection`. The old warning "no remote record currently exposes an SSH
connection tuple" can therefore appear even when a live SSH or Mosh attachment
has a resolved `focus_target`. That warning alone does not prove focus is broken.

If no attachment evidence is reported, attach the remote session through a local
SSH or Mosh tmux pane and refresh. Persistent sessions survive disconnection,
but their transport binding does not. Reconnection discovers the new client on
subsequent scans. Incomplete inspection can also leave attachment evidence
absent. If evidence exists without a binding, inspect the local transport panes
for a missing match or multiple matches. `tmux-agent explain <id-or-pane>` shows
the selected record; an alias-level check may include other unbound records.

A snapshot binding still needs live revalidation during focus. Inner tmux
selection also needs structured `[[machine]]` configuration and the peer's
`remote_tmux_focus_v1` capability. Raw collectors, older peers, and some explicit
or legacy bindings support only outer transport focus. When testing, verify the
requested inner window and pane, not just the outer SSH or Mosh pane. See
[remote focus](remote-machines.md#remote-focus) for confirmation and failure behavior.

Remote focus requires a unique local tmux pane carrying the matching live SSH
or Mosh session attachment, an ordinary-terminal mosh pane whose client process names the
configured remote and whose normalized title matches, or a mirror pane with
the public remote marker options. Multiple matching panes are rejected.
Ordinary-terminal mosh focus never uses title alone, never claims an explicitly
marked pane, and does not write a binding. Use `tmux-agent explain <id-or-pane>`
and `tmux-agent doctor` to inspect the derived target without exposing pane
contents.

For an already attached nested tmux session, tmux-agent can adopt one live mosh
pane when its configured destination matches and its title uses the
`[mosh] · ...` nested shape for the selected agent. The pane may be unmarked or
carry a stale complete binding for a different host; the live mosh destination
must prove the selected remote before both markers are replaced. An ordinary
`[mosh] title` pane is not adopted. Existing bindings for other sessions on the
host do not block a unique match. Zero or multiple matches remain unchanged.

For a detached default-server tmux session, tmux-agent can recover one idle,
unmarked mosh shell when both its configured destination and displayed working
directory match the remote record. It verifies the resulting agent title before
writing a binding. Another session binding on the host does not block a unique
alias and working-directory match. Named servers, zero matches, and multiple
matches are not guessed.

For other nested remote tmux cases, bind the correct local transport pane
initially. Run the command on the local machine, not inside the remote tmux session:

```sh
tmux-agent remote bind <configured-remote> <remote-tmux-session> --pane <local-pane-id>
```

Use `tmux-agent remote bindings` to inspect current mappings and `tmux-agent
remote unbind --pane <local-pane-id>` to remove one. A stale remote session name
self-heals during focus when exactly one live pane is already bound to that host
and its normalized title matches the selected agent. A stale host and session
pair can also self-heal when one live mosh destination and nested title match.
If precise recovery still fails, one live, non-UI pane with a complete binding
for that host can be focused as the outer transport. Its session marker may
refer to another nested tmux session. tmux-agent does not switch the inner
session or change either marker, and the UI reports the degraded result.
Several same-host bindings remain ambiguous. Dead panes, UI panes, and partial
bindings do not qualify.

## A completion remains visible

Activate the row with `Enter` or a left click, or run:

```sh
tmux-agent acknowledge <id-or-pane>
```

Acknowledgement remains effective until that agent begins another active
turn.

## Handoff delivery fails or is unconfirmed

Inspect discovery and the retained result locally:

```sh
tmux-agent find --json
tmux-agent handoff sent <handoff-id> --json
```

The second command includes full message text; redact it before sharing.

- **No match or ambiguity:** narrow by machine, provider, session or `--cwd`,
  then use the full returned ID. Bare pane IDs are local unless `--machine`
  is given. `--repo` and `--branch` are task metadata, not target filters.
- **Copy mode, blocked, unknown or working:** resolve the reported condition
  first. Do not cancel someone else's copy mode or bypass a permission prompt.
  `--allow-working` is only for intentional input queuing by a known provider.
- **Unsupported command or capability:** verify the actual binary on both
  machines and `handoff_v1` in `tmux-agent list --json`. An installed skill
  cannot supply a command missing from the binary. Raw collectors lack control.
- **Different server or stale recipient:** check the machine's recipient
  `config` path and rediscover after pane moves or process restarts. Do not fix
  a handoff routing error by changing an outer Mosh focus binding.
- **SSH timeout or `unconfirmed`:** the message may have arrived. Inspect the
  recipient and retry only with the same ID, recipient and exact header/body.
  A pending/partial-submission result requires inspection, not another send.
  Do not delete claims or generate a new ID to bypass uncertainty.
- **`sending` after interruption:** no final result was recorded. Treat the
  outcome as unknown; first check whether the original command is still running.
- **Skill or runtime permission denied:** check the sender's sandbox can reach
  the resolved skill path, runtime paths and SSH. Do not disable it automatically.

`delivered` confirms paste and Enter, not the agent's execution or completion.
Check its transcript/report for the actual response. Full semantics are in
[Agent handoff](handoff.md#reading-a-result).

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
