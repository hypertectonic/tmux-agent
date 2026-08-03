# Changelog

All notable changes to `tmux-agent` will be documented in this file.

The project uses semantic versioning.

## Unreleased

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
