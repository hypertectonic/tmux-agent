# Architecture

tmux-agent is local-first. One daemon per tmux server collects local state,
serves a user-only Unix socket, and optionally aggregates explicitly configured
SSH peers.

```text
provider process -> local scanner -> local daemon -> terminal UI
owned PTY runner --------^              ^
                                        |
remote daemon watch -> SSH collector ---+
```

## Local discovery

The scanner combines tmux pane metadata, foreground process groups, ordinary
TTY process groups, and owned PTY records. Captured terminal surfaces are used
only on the machine that owns them.

Codex, Claude, OpenCode, Grok, and Pi each have a typed detector backed by
minimal synthetic fixtures. Detectors emit semantic state and derived evidence
details, not the source terminal content. State stabilization prevents one
quiet frame from immediately erasing stronger working or blocked evidence.

The owned PTY runner transparently forwards input, output, resize events,
signals, job control, and child exit status. Its mode `0600` heartbeat record
contains only process identity, terminal identity, working directory, derived
state, and detector diagnostics. Records expire after three missed heartbeats.

## State and attention

Semantic states are `working`, `blocked`, `idle`, and `unknown`. Attention is
ordered as:

```text
blocked > done > working > idle > unknown
```

`done` is derived when an active agent becomes idle while its tmux window is
not visible. Activating the row or using `acknowledge` marks the completion
seen. Codex goal achievements use the same explicit acknowledgement boundary.

## Subagents

Process-backed agents are linked to the nearest discovered ancestor in the
process tree. Codex in-process children use bounded metadata from local rollout
files. Only thread identity, parent identity, nickname, working directory, and
lifecycle timestamps participate in discovery. Rollout content never enters a
snapshot.

Nested parent identity is retained while visible children are active. Finished
children remain visible for 30 seconds.

## Local IPC and protocol

The daemon listens on a mode `0600` Unix socket. Requests and responses are one
JSON object per line:

- `snapshot` returns the current state
- `watch` sends the initial snapshot and later revisions
- `acknowledge` marks a completion seen
- `shutdown` stops the daemon cleanly

The current federation protocol version is `3`. Live federation requires equal
protocol versions on all machines.

Peer status contains connection state, a bounded error message, application
version, protocol, and capabilities. It intentionally has no freshness
timestamp because snapshots are emitted on state changes, not as connection
heartbeats.

## SSH federation

A structured machine produces a hardened command equivalent to:

```text
ssh -T -o BatchMode=yes -o ConnectTimeout=5 \
  agent@build-host.example.ts.net \
  /absolute/path/tmux-agent watch --jsonl --local-only
```

The central daemon verifies protocol compatibility, namespaces remote IDs with
the configured alias, merges derived records, and removes them when the stream
fails. SSH provides encryption, authentication, host keys, and routing. There
is no application TCP listener or shared application credential.

Federation snapshots never include captured pane contents, screen buffers,
prompts, reasoning, rollout events, goal objectives, or raw process command
lines.

## Focus and presentation

Local records focus their stored tmux session, window, and pane. Remote focus
matches the two endpoints of an established SSH connection to a local tmux
pane. Public mirror markers provide an explicit fallback:

```text
@tmux_agent_remote_host
@tmux_agent_remote_session
```

Execution-host presentation is stored locally in `@tmux_agent_host` and
`@tmux_agent_host_color`. These values are not added to federation snapshots.

## Security boundaries

- Runtime directories use mode `0700` and files use mode `0600`.
- Remote transport remains SSH.
- Captured pane contents remain on the owning machine.
- Normal federation does not transport child transcript content.
- Opening a remote Codex child uses a separate, explicit SSH viewer session.
- Ambiguous process, parent, or focus matches are rejected instead of guessed.
