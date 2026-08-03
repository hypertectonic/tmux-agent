# Security policy

`tmux-agent` inspects local process metadata and visible tmux screens, and can
connect to explicitly configured machines over SSH. Security and privacy
reports are treated as high priority.

## Supported versions

There is no public release yet. Until the first release is published, security
fixes are made on the active development branch.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. After the GitHub
repository is created, use its private vulnerability reporting feature. If
private reporting is unavailable, contact the maintainer through a private
channel listed on the repository profile.

Include only the minimum evidence needed to reproduce the issue. Do not send:

- Credentials, tokens, cookies, or SSH private material.
- Raw terminal transcripts or captured pane contents.
- Private hostnames, Tailnet inventory, or complete process command lines.
- Unredacted configuration files.

It is useful to include the affected version, operating system, architecture,
tmux version, agent provider, local or SSH topology, and a minimal synthetic
reproduction.

## Security boundaries

- Daemon IPC uses a user-only Unix socket.
- Runtime and state data use user-only filesystem permissions.
- Federation uses the user's existing SSH trust and host-key policy.
- Captured pane contents are never included in federation snapshots.
- Remote transcript content is transmitted only through an explicitly opened
  read-only SSH viewer and is not persisted on the central machine.
- The project does not install aliases or replace provider executables.

These boundaries are security invariants. Changes that weaken them require
explicit design review and documentation.
