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

The daemon refreshes the global process table and process-derived socket
inventory at most once per second. Every scan still reads current tmux pane
metadata and projects those panes against the latest process inventory.
Candidate panes in a window displayed by any attached tmux client are captured
on every normal scan. Candidate panes in hidden windows reuse their last
successful screen and are captured on the first scan at least one second after
the previous attempt, normally about 1.0 to 1.3 seconds with the default 300 ms
scan interval. Newly eligible panes and panes whose foreground process identity
changes are captured immediately. A failed hidden capture discards the cached
screen and waits for the same background interval before retrying. Replaying a
cached quiet screen does not count as a new idle observation, while current
tmux metadata and title evidence are still evaluated on every scan.

Process starts, exits, and SSH connection changes can therefore take about one
second to appear. Tmux metadata and screen-derived state in displayed windows
remain on the normal scan cadence.

Codex, Claude, OpenCode, Grok, OMP, and Pi each have a typed detector backed by
minimal synthetic fixtures. Detectors emit semantic state and derived evidence
details, not the source terminal content. State stabilization prevents one
quiet frame from immediately erasing stronger working or blocked evidence.

OMP v17.3.4 enables state titles by default. Its exact `π <separator>
<label>` title is the primary state signal. The scanner stores only `<label>`,
so the 80 ms working spinner does not change the record title or publish a new
snapshot. If OMP disables or overrides state titles, its detector falls back
to narrow visible status and permission markers.

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
Within the idle bucket, top-level agents sort by the newer of their state-change
time and their last successful focus through tmux-agent. The daemon keeps focus
times in memory for its own tmux server and discards them when agents disappear
or the daemon restarts. Other attention buckets and subagent ordering keep their
existing location and lifecycle ordering.

## Subagents

Process-backed agents are linked to the nearest discovered ancestor in the
process tree. Codex in-process children use bounded metadata from local rollout
files. Only thread identity, parent identity, nickname, working directory, and
lifecycle timestamps participate in discovery. Rollout content never enters a
snapshot.

The scanner owns provider-neutral process-tree linking, ancestry restoration,
and the 30-second finished-child retention window. Rollout discovery and
metadata parsing stay on the Codex side of the scanner boundary, with open-file
inspection isolated in the Codex evidence adapter. A focused, deterministic
Codex ownership reconciler then applies exact thread identity, recovered root
bindings, process/in-process deduplication, nesting, and fail-closed ambiguity
rules. It owns the short-lived Codex binding state but performs no tmux,
process-table, `lsof`, or filesystem I/O.

Nested parent identity is retained while visible children are active. Finished
children remain visible for 30 seconds.

## Local IPC and protocol

The daemon listens on a mode `0600` Unix socket. Requests and responses are one
JSON object per line:

- `snapshot` returns the current state
- `watch` sends the initial snapshot and later revisions
- `acknowledge` marks a completion seen
- `mark_used` records a successful top-level focus for local idle ordering
- `shutdown` stops the daemon cleanly

The current federation protocol version is `3`. Live federation requires equal
protocol versions on all machines. `mark_used` is local IPC metadata: its
timestamps are neither serialized nor included in `--local-only` federation
snapshots, so it does not change the federation protocol.

Peer status contains connection state, a bounded error message, application
version, protocol, and capabilities. It intentionally has no freshness
timestamp because snapshots are emitted on state changes, not as connection
heartbeats.

The terminal UI keeps one `watch` connection open. The daemon releases that
connection when the UI closes or replaces its watch. The UI redraws after
input, resize, snapshot, message, working animation, or a visible unfinished
subagent's one-second elapsed-time change. A user-managed sidebar checks whether
its tmux window is active in an attached session. Hidden sidebars keep their
latest snapshot, search, and selection but pause drawing, working animation,
and elapsed-time ticks until the window is visible again. An explicit numeric
selection permits one hidden redraw so sibling pane buffers stay synchronized
without waiting for their next visibility probe. Visibility never changes
search, selection, scrolling, or activation state. Popups remain visible for
their whole lifetime, and `r` replaces the current watch with a freshly
connected stream and its initial snapshot.

Numeric shortcuts publish only their explicit selected agent ID to sibling
persistent UIs in the same tmux server. Activation focuses first; selection
fanout then runs in the background and wakes recipient panes concurrently.
Ordinary navigation and tmux focus changes remain local to each UI process.
Successful Enter, mouse, and numeric activation share the same focus seam. That
seam reports usage on a best-effort basis only after tmux focus succeeds,
advancing the daemon watch so every persistent UI on the same tmux server
receives the reordered snapshot. An unavailable usage operation never turns a
successful focus into a failed activation.

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

The release lifecycle gate combines these component contracts without giving
one component another's authority. Rust tests exercise update discovery,
downgrade prevention, locking, verification, atomic activation, daemon restart
recovery, version selection, and rollback. Shell integration tests exercise
bootstrap and both launchers. The release workflow then uses the checksum-pinned
public v0.3.0 packages and a genuinely newer candidate package to exercise
fresh and upgrade layouts in standalone and TPM modes on every supported
release target. The v0.3.0 runtime remains `current` during TPM checkout
migration while the candidate becomes `manager`; lifecycle commands therefore
remain available after rolling back to a runtime that predates them.

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

Remote lifecycle changes remain outside the federation protocol. A user may
run `tmux-agent update`, `versions`, or `rollback` through an ordinary explicit
SSH command, but neither the central daemon nor a configured peer can initiate
that command.

Federation snapshots never include captured pane contents, screen buffers,
prompts, reasoning, rollout events, goal objectives, or raw process command
lines.

## Focus

Local records focus their stored tmux session, window, and pane. Remote focus
matches the two endpoints of an established SSH connection to a local tmux
pane. For an ordinary remote terminal over mosh, focus can instead resolve one
live, unmarked, non-UI pane whose local `mosh-client` process title names the
configured remote and whose normalized pane title matches the selected record.
The resolver rejects zero or multiple candidates, does not persist process
arguments, and does not write a binding. Public mirror markers provide an
explicit fallback for remote tmux:

```text
@tmux_agent_remote_host
@tmux_agent_remote_session
```

`tmux-agent remote bind` and `remote unbind` manage these pane-local markers.
This gives nested tmux over mosh an exact local focus target without screen or
unmarked title inference. An exact host and session binding is always used
first. If a remote tmux session is recreated under a new name, focus can update
the session marker when exactly one live, non-UI pane is already marked for
that host and its normalized title matches the selected agent.

Without an exact or repairable binding, focus can adopt one live, non-UI mosh
pane whose client destination matches the configured remote and whose title
uses the established nested tmux shape, `[mosh] · ...`, for the selected agent.
The pane may be unmarked or carry a complete stale binding for a different
host; the live client destination must prove the selected remote before both
markers are replaced. An ordinary `[mosh] title` pane is not adopted as tmux. A
binding for another session on the same host does not disqualify a unique
candidate. Zero or multiple candidates leave all markers unchanged.

If the selected default-server session is detached, focus can instead recover
through one unmarked mosh shell whose client destination and shell
working-directory title both match the remote record, including when another
session on that host is already bound. Focus selects the exact remote window
and pane, attaches the remote tmux client, waits for the local pane to adopt the
selected agent title, then writes the binding and selects the pane. Zero or
multiple shell matches, named remote servers, and an unverified attach all fail
closed. The daemon protocol and persisted state do not carry the binding.

## Security boundaries

- Runtime directories use mode `0700` and files use mode `0600`.
- Federation transport remains SSH. An explicit focus binding may point at a
  local SSH or mosh pane.
- Captured pane contents remain on the owning machine.
- Normal federation does not transport child transcript content.
- Opening a remote Codex child uses a separate, explicit SSH viewer session.
- Ambiguous process, parent, or focus matches are rejected instead of guessed.
