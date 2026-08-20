# Changelog

All notable changes to `tmux-agent` will be documented in this file.

The project uses semantic versioning.

## Unreleased

## [0.8.0] - 2026-08-20

### Added

- Bind an SSH or Mosh transport pane to a specific nested remote tmux session
  with `tmux-agent remote bind`, with commands to inspect and remove bindings.

### Changed

- Batch pane captures, cache macOS terminal resolution, and reuse the global
  process inventory for one second while keeping current pane metadata fresh.
- Keep displayed-window captures on the normal scan cadence while reusing
  hidden-window screens for one second. Replayed screens do not advance idle
  confirmation without fresh evidence.

### Fixed

- Exit a daemon after its tmux server remains missing for three scans, remove
  its runtime socket, and stop its process discovery and remote collectors.
- Focus the intended nested remote tmux pane when an explicit transport
  binding is configured.

## [0.7.0] - 2026-08-17

### Added

- Detect Oh My Pi (OMP) as a distinct provider in tmux and ordinary terminals,
  with typed screen-state evidence, stable spinner-title normalization, and a
  dedicated magenta provider badge.
- Run OMP in an owned PTY with `tmux-agent omp [args...]`, preserving argument
  forwarding and the child exit status.
- Show the real tmux-agent workflow in the README with a sanitized terminal
  recording that demonstrates filtering and switching between Codex and OMP.

### Changed

- Keep Pi and OMP identification separate by recognizing only the `omp`
  executable and official OMP package entrypoints.

## [0.6.0] - 2026-08-15

### Added

- Activate the first ten top-level sessions with `1` through `9` and `0`.
  Provider-adjacent keycaps show the available shortcuts, and explicit numeric
  selection stays synchronized across persistent UIs on the same tmux server.
- Keep recently used idle sessions near the top of the idle bucket. Successful
  focus through tmux-agent updates an in-memory daemon timestamp shared by its
  local UIs, while other attention buckets and subagent ordering stay unchanged.

### Changed

- Remove tmux-agent's pane-host badge presentation metadata, palette, and
  reconciliation. Pane titles, `@pane_label`, SSH transport discovery, remote
  routing markers, and focus behavior remain supported.
- Refresh the README interface overview to match the current header, shortcut
  keycaps, row styling, peer status, and footer controls.

## [0.5.0] - 2026-08-15

### Added

- Filter UI sessions as you type with `/` while preserving the existing
  single-key navigation controls outside search mode.

### Changed

- Make persistent sidebars event-driven, using one daemon watch per UI and
  redrawing only when visible state changes. In a 14-sidebar workload this
  reduced aggregate UI CPU use from 10.15% to 2.17%, a 78.6% reduction.

### Fixed

- Keep Grok rows stable by showing the working-directory name instead of the
  changing terminal activity title.
- Clear the active search after successfully switching to a selected session,
  so returning to the sidebar restores the full list.
- Keep topology-changing redraws compatible with asynchronous terminal input
  and reconnect persistent sidebars after daemon restarts.

## [0.4.1] - 2026-08-10

### Changed

- Isolate process-owned Codex rollout discovery behind the Codex evidence
  boundary instead of the generic tmux adapter.
- Move Codex subagent ownership state and reconciliation policy behind a
  focused deterministic module while keeping provider-neutral process linking,
  ancestry restoration, and completion retention shared.

### Reliability

- Preserve exact and resumed identity precedence, retained root bindings,
  nesting, process/in-process deduplication, completion suppression, and
  fail-closed ambiguity at a focused test seam.

## [0.4.0] - 2026-08-09

### Added

- A checkout-independent `tmux-agent update` command with anonymous stable
  release discovery, immutable version-pinned downloads, native package and
  checksum verification, atomic activation, restart rollback, and explicit
  exact-version prerelease updates.
- Checkout-independent managed-version listing and fail-safe rollback commands,
  plus direct standalone and existing TPM layout migration to the stable
  launcher without discarding the previous binary.
- A separately verified lifecycle controller keeps update, version listing, and
  roll-forward recovery available after selecting a legacy runtime binary. A
  verified newer controller is retained when the runtime moves to an older or
  intermediate version.
- The stable standalone launcher has a versioned ownership header, supports the
  legacy lifecycle aliases, upgrades independently of its managed binaries, and
  is removed safely by the uninstaller.
- A cross-target release lifecycle gate validates fresh standalone and TPM
  installs plus direct-binary and checksum-pinned v0.3.0 TPM upgrades. Public
  guidance now covers explicit update, migration, rollback, failure recovery,
  checkout ownership, and user-controlled SSH procedures.

### Fixed

- Recover picker-resumed Codex root identity from its process-owned rollout
  metadata so subagents stay attached to the correct session, while refusing
  ambiguous or directory-only parent matches.
- Preserve a validated metadata-absent pre-self-update TPM runtime as a native
  rollback target without moving `current` or granting it lifecycle-controller
  capability; ambiguous and partially published layouts fail closed.

### Security

- Release archives now carry platform and launcher compatibility metadata, and
  self-update rejects unsafe entries, mismatched checksums, targets, protocols,
  and embedded binary versions without replacing the active binary. Network
  client configuration is isolated, and transfer, extraction, and binary
  verification work is bounded.
- Rollback rejects missing, incompatible, non-native, symlinked, or corrupt
  managed targets and restores the prior activation if daemon restart fails.
- Direct-binary migration rejects symlinked store collisions and accepts a
  same-version binary only when it exactly matches the verified release, leaving
  custom binaries untouched.

## [0.3.0] - 2026-08-04

### Added

- Pi harness discovery in tmux and ordinary terminals, typed activity and
  input-state detection, a provider badge, and the `tmux-agent pi` owned-PTY
  shortcut.

### Fixed

- Restore the UI footer key hints three seconds after transient action feedback,
  while keeping daemon connection errors visible until recovery.

## [0.2.0] - 2026-08-03

### Added

- Local tmux and ordinary-terminal discovery for Codex, Claude, OpenCode, and
  Grok.
- Typed provider detectors backed by synthetic behavior fixtures.
- A local daemon, terminal UI, owned PTY runner, and SSH federation.
- Codex goal status, completion acknowledgement, and nested subagent views.
- A TPM plugin with verified release installation, update, rollback, and
  diagnostics.

### Security

- Federation snapshots exclude captured pane contents, prompts, reasoning,
  rollout events, raw command lines, and goal objectives.
- Remote transport uses the user's existing SSH trust and host-key policy.
