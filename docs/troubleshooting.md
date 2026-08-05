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

- Confirm the provider is Codex, Claude, OpenCode, Grok, or Pi.
- Run `tmux-agent scan --json` on the machine that owns the session.
- Confirm the agent process is in the foreground process group.
- Inside tmux, make the pane visible once so the detector can inspect its
  current terminal surface.
- Outside tmux, use an owned PTY shortcut for screen-based state detection.

## A session shows unknown

Process-only discovery proves that a supported provider exists but cannot
always prove its current activity. Run the provider inside tmux or through
`tmux-agent codex`, `tmux-agent claude`, `tmux-agent opencode`,
`tmux-agent pi`, or the generic `tmux-agent run -- <command>` wrapper.

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

## A completion remains visible

Activate the row with `Enter` or a left click, or run:

```sh
tmux-agent acknowledge <id-or-pane>
```

Acknowledgement remains effective until that agent begins another active
turn.

## Roll back

```sh
tmux-agent plugin versions
tmux-agent plugin rollback <version>
```

Rollback uses an already installed, checksum-verified version.
