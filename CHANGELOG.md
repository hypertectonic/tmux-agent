# Changelog

All notable changes to `tmux-agent` will be documented in this file.

The project uses semantic versioning.

## Unreleased

### Added

- A checkout-independent `tmux-agent update` command with anonymous stable
  release discovery, immutable version-pinned downloads, native package and
  checksum verification, atomic activation, restart rollback, and explicit
  exact-version prerelease updates.

### Security

- Release archives now carry platform and launcher compatibility metadata, and
  self-update rejects unsafe entries, mismatched checksums, targets, protocols,
  and embedded binary versions without replacing the active binary. Network
  client configuration is isolated, and transfer, extraction, and binary
  verification work is bounded.

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
