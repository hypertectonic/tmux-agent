# Changelog

All notable changes to `tmux-agent` will be documented in this file.

The project uses semantic versioning.

## Unreleased

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
