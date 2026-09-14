tmux-agent v0.10.0 adds agent discovery and direct handoffs, portable skills for
handoffs and interactive task windows, and visible activity from working
subagents.

## Highlights

- Find running agents with `tmux-agent find` and send scoped assignments with
  `tmux-agent handoff send`, locally or over configured SSH. The message and
  its provenance header appear in the recipient's input and transcript.
- Resolve exactly one recipient and revalidate its pane, provider, foreground
  process lifetime and state before submission. Blocked, unknown and copy-mode
  targets are refused; working targets require an explicit override.
- Inspect bounded sender history with `tmux-agent handoff sent`. Confirmed
  same-ID retries report duplicates; potentially partial submissions are not
  automatically replayed.
- Use the optional, harness-neutral handoff and workspace skills. The workspace
  skill creates native tmux task windows, starts an interactive agent in the
  intended checkout, and guides assignment delivery, monitoring and cleanup.
  It respects existing tmux hooks and needs no particular dotfiles or worktree
  manager. Requests for native subagents remain separate.
- See activity while a recognized subagent is working, even when its parent
  is idle. A working-subagent count stays visible beside status or goal
  information without changing the parent's completion or acknowledgement.

## Compatibility and delivery limits

Federation remains on protocol 4, with the additive `handoff_v1` capability.
Existing protocol-4 peers can still be monitored, but both sender and recipient
need handoff support to exchange messages. Package versions need not be
identical. The managed launcher protocol is unchanged.

Remote handoffs require structured `[[machine]]` configuration and working SSH
control access. A machine's optional `config` path selects the same recipient
tmux configuration for collection and control, including named or custom
servers. Explicit missing config files now fail instead of using defaults.
Handoffs use SSH even when the visible attachment uses Mosh; no outer-pane
binding or window switch is required. Sending back needs its own configured
SSH route.

`delivered` confirms terminal submission, not task acceptance or completion.
There is no queue or background retry. Provider state can change between
validation and input, and custom persistent shell loops can replace a provider
without changing its recorded process-group lifetime. Inspect uncertain
outcomes before retrying. Full messages are retained in bounded sender history
and may appear in provider transcripts; they are not part of federation
snapshots.

Workspace monitoring belongs to the initiating agent and stops if it stops.
Finished windows and worktrees remain until cleanup is requested.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
tmux-agent --version
```

Update each participating remote explicitly, then the central machine.
Restart existing daemons, collectors and UI processes, and verify peer
capabilities with `tmux-agent list --json`. A new binary on disk does not
replace running processes. tmux-agent does not orchestrate remote updates.

Install the optional skills separately from `skills/tmux-agent-handoff/` and
`skills/tmux-agent-workspace/` in the matching source version. Binary updates
do not install skills. See the installation guide for copy/symlink and reload
instructions for your harness.

TPM's `prefix + U` updates the plugin checkout, not the packaged binary.

## Downloads

Archives are provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and
Linux ARM64. Verify downloaded archives with `SHA256SUMS`. Release assets have
signed build provenance.

## Documentation

See the
[handoff guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.0/docs/handoff.md),
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.10.0/SECURITY.md).
