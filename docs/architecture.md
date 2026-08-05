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

## Plugin and managed-binary compatibility

The TPM checkout owns compatibility repair and fallback installation; the
packaged binary owns update, version listing, and rollback, while the managed
binary store owns executable versions. `COMPATIBILITY` defines the launcher
protocol and minimum binary version required by a checkout. Every published
managed version directory records its binary version and launcher protocol in
its own `COMPATIBILITY` metadata. Management-capable packages additionally
record management protocol `1`. The store has two independently atomic
selections: `current` is the runtime binary, while `manager` is the verified
lifecycle controller used only for update, version listing, and rollback.
Compatibility requires all of the following:

- `current` names the verified binary in its immutable `versions/<version>`
  directory;
- the binary reports the same semantic version as that directory and metadata;
- the installed and checkout launcher protocols are equal; and
- the binary version is at or above the checkout's minimum.

`manager` must use the same constrained relative or data-directory-absolute
target shape as `current`, name a native installed package with management
protocol `1`, and pass the same binary and metadata validation. Rollback changes
only `current`; keeping `manager` on a management-capable package ensures that
an older runtime without lifecycle subcommands cannot strand the installation.
Controller selection is monotonic: bootstrap and packaged update keep an
already verified `manager` when its semantic version is equal to or newer than
the candidate.

Bootstrap holds `~/.local/share/tmux-agent/.install.lock` (under the configured
data directory) while it rechecks compatibility, publishes a complete version
directory, and atomically renames a relative `current` symlink. Rollback uses
the same lock and Rust activation/restart-recovery operation as update. Under
that lock, bootstrap can add native target and launcher metadata to the exact
pre-self-update TPM layout: an in-store, metadata-absent `current` runtime with
no `manager`. It publishes `TARGET` first and `COMPATIBILITY` as the commit
point, so an exact `TARGET`-only interruption is rerunnable without granting
the legacy runtime management capability. Other partial or ambiguous layouts
fail closed. The
packaged `tmux-agent update`
command also uses this contract without reading a checkout: mutable GitHub
metadata is used only to discover and validate the latest stable semantic
version, while archive and `SHA256SUMS` downloads use immutable version-pinned
URLs. Release archives carry explicit target and launcher compatibility
metadata in addition to the binary version. All entries must be regular files
from a fixed allowlist and remain within transfer and expansion bounds before
an immutable version directory is published. Binary-version probes are also
time- and output-bounded.

The standalone installer publishes a small checkout-independent launcher that
routes runtime commands to `current` and lifecycle commands to `manager`. When it
finds a direct binary at the launcher path, bootstrap imports that binary under
the shared lock, records its native target and launcher protocol, and preserves
it as a recovery version before the launcher path is atomically replaced. The
same-version case is deferred until a checksum-verified package has been staged;
the direct binary must be byte-identical to that package. Store and version
directory collisions must be real in-store directories whose binary and
metadata files are regular non-symlinks. The launcher's exact versioned header
distinguishes an older official launcher, which can be replaced atomically
without treating it as a direct binary. The same header is required before
uninstall removes a configured standalone launcher path. The
checkout launcher retains compatibility repair for TPM, but its legacy version
listing and rollback commands delegate to `manager`. Normal daemon, UI, and
agent commands continue to execute `current`.

Thus an older checkout treats a newer binary with the same launcher protocol
as current and cannot replace it with the checkout-pinned fallback. A missing,
below-minimum, corrupt, or protocol-incompatible managed binary is repaired
from that pinned fallback. Self-update activates only a completely verified
version, restarts the daemon afterward, and restores the previous activation
if the restart fails. Managed listing and rollback fail closed when a selected
directory, binary, platform record, or compatibility record is missing,
incompatible, symlinked, or corrupt.

This launcher protocol is separate from the daemon federation protocol. TPM's
`prefix + U` continues to update the plugin checkout; it is not a managed-binary
update command.

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
