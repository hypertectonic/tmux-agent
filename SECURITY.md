# Security policy

`tmux-agent` inspects local process metadata and visible tmux screens, and can
connect to explicitly configured machines over SSH. Security and privacy
reports are treated as high priority.

## Supported versions

| Version | Supported |
| --- | --- |
| 0.3.x | Yes |
| 0.2.x | No |

Security fixes are prepared on the active development branch and released from
the stable release line.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub private
vulnerability reporting when it is available. Otherwise, contact the
maintainer through a private channel listed on the repository profile.

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
- Packaged updates discover only a validated stable semantic version from the
  canonical public repository, download assets through immutable version-pinned
  HTTPS URLs, and verify their checksum, file allowlist, platform metadata,
  launcher protocol, and embedded binary version before atomic activation.
  Implicit curl/wget configuration and netrc credentials are disabled, and
  transfer, extraction, and binary-version probes have explicit bounds.
- Update failures preserve the previous usable binary; a daemon restart
  failure restores the previous activation.
- Managed rollback revalidates the installed binary, native target, and
  compatibility metadata under the shared installation lock, and restores the
  prior activation on restart failure. Standalone migration preserves a
  locally installed direct binary before atomically replacing its path with
  the checkout-independent launcher; migration failure leaves that path
  untouched. A direct binary with the checkout version must byte-match the
  checksum-verified release before publication, and existing store collisions
  must be real directories with regular, non-symlink metadata and binaries.
- The `manager` selection is a separately validated native, management-capable
  package used only for update, listing, and rollback. Rollback changes only
  `current`, so selecting a legacy runtime cannot make lifecycle recovery depend
  on that runtime. Bootstrap and update never replace a verified controller with
  an older candidate. Invalid controller links or metadata fail closed.
- Legacy TPM metadata migration accepts only a canonical, executable in-store
  `current` runtime at or above the checkout compatibility floor when `manager`
  and both metadata files are absent. An exact native `TARGET`-only state is the
  sole resumable interruption; other partial, symlinked, or ambiguous states
  fail closed, and migrated runtimes are not marked management-capable.
- The standalone launcher's exact three-line format marker is the ownership
  boundary for in-place launcher upgrades and removal. Uninstall leaves
  unrelated executables and every symlink at the configured launcher path
  untouched.

These boundaries are security invariants. Changes that weaken them require
explicit design review and documentation.
