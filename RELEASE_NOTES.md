tmux-agent v0.11.0 adds Claude child rows and unmet Codex goal outcomes, and
improves remote-focus diagnostics.

## Changes

- Show recognized Claude children beneath their verified parent, independently
  of the visible child panel. Selecting one focuses the parent rather than
  opening a transcript or sending input to the child.
- Display budget-exhausted Codex goals as `Goal unmet`, or `goal!` in a narrow
  view. Working and blocked activity retain precedence; activation or `a`
  acknowledges the outcome. Ordinary `done` still means the turn finished,
  not that the goal succeeded.
- Report current cached SSH and Mosh bindings in `doctor`, including missing
  attachment evidence and disconnected peers. A binding or control capability
  is not a claim that inner-pane focus was verified.
- Clarify the optional workspace skill: an explicitly requested worker may
  accept folder trust for its verified checkout, but unexpected paths,
  authentication and broader permission requests still require attention.

Claude discovery reads bounded local session metadata and transcript lifecycle
events. Raw contents remain on the owning machine and are not included in
federation snapshots. Quiet children become `unknown` after 30 seconds without
events, completed children remain for 30 seconds, and unknown children expire
after 30 minutes. Missing or unsupported metadata leaves parent-only detection
in place. This remains evidence-limited detection, not a provider API.

## Compatibility

Federation remains on protocol 4. Unmet goals use an additive field, so older
readers keep the agent record but do not display the new outcome. Update all
participating daemons, watchers and UIs for the new behavior. No new capture
loop, remote service or provider hook is introduced.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
tmux-agent --version
```

Update configured remote machines explicitly, then the central machine.
Restart existing daemons, collectors and UI processes; replacing a binary on
disk does not replace running code. If returning from a private experiment,
restore official local and remote launch paths before restarting.

TPM's `prefix + U` updates the plugin checkout, not the packaged binary.
Binary updates do not install the optional skills. Take those from the same
release and follow the installation guide when updating them.

## Downloads and documentation

Archives are available for macOS Apple Silicon, macOS Intel, Linux x86-64 and
Linux ARM64. Verify archives with `SHA256SUMS`; release assets have signed build
provenance.

See the [installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.11.0/docs/installation.md)
and [remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.11.0/docs/remote-machines.md).
