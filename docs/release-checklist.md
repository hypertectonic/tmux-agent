# Release checklist

This checklist is the release-readiness contract for tmux-agent. It supplements
the signed tag and draft-release workflow; it does not authorize a release.

## Before tagging a candidate

- [ ] `VERSION`, `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and curated
  `RELEASE_NOTES.md` describe the same candidate.
- [ ] The candidate version is newer than v0.3.0. A same-version reinstall is
  not accepted as evidence of a cross-version upgrade.
- [ ] `scripts/check-release-readiness`, `cargo test --locked`, Clippy, shell
  formatting/static checks, and `tests/run-shell-tests` pass.
- [ ] The four supported targets remain present in both CI and release
  matrices: Apple Silicon macOS, Intel macOS, x86-64 Linux, and ARM64 Linux.
- [ ] No updater behavior grants automatic-update or remote-execution
  authority.

A draft candidate tag may append `-rc.N` to the final `VERSION`, for example
`v0.4.0-rc.1` for `VERSION` `0.4.0`. The workflow keeps that Release private,
builds archives with the final version, and exercises the exact commit that
will receive the final tag. Candidate tags are never moved or published.

## Lifecycle coverage contract

| Gate | Required evidence |
| --- | --- |
| Rust update tests | Pinned downloads, verification, no-op, downgrade prevention, shared locking, atomic activation, daemon restart, restart rollback, recovery, version listing, and rollback validation |
| Installer and launcher integration | Bootstrap locking and failure recovery, standalone direct-binary migration, TPM compatibility repair, `current`/`manager` independence, legacy aliases, and interrupted v0.3-style metadata migration |
| Documentation smoke | Public command help plus the explicit user-controlled SSH `update`, `versions`, and `--version` procedure; no tmux-agent remote orchestration |
| Release-target build | A native binary and release package for every supported target, with matching embedded version and target metadata |
| Release lifecycle smoke | Fresh standalone install, v0.3.0 direct standalone upgrade, fresh TPM install, and exact v0.3.0 TPM layout migration using the real candidate package on every target; launcher-routed no-op, downgrade prevention, live-lock serialization, and restart-failure recovery |

`tests/release-lifecycle-smoke.sh` is the final cross-version gate. The release
workflow downloads v0.3.0 from its immutable versioned release path, checks the
published checksum against a repository-pinned value, and verifies the archive
still has the historical metadata-absent layout before exercising it. Network
or baseline-integrity failures stop in the input step or preflight and are not
reported as lifecycle regressions.

For the v0.3.0 TPM migration, the smoke test must prove that:

- [ ] checkout bootstrap preserves v0.3.0 as `current` and installs the
  candidate as `manager`;
- [ ] migration adds native `TARGET` and launcher metadata without granting the
  v0.3.0 binary management capability;
- [ ] `tmux-agent versions` is served by the candidate controller;
- [ ] rollback to the candidate and back to v0.3.0 atomically changes only
  `current`, keeps `manager`, and restarts the selected daemon; and
- [ ] fresh and upgraded standalone stores use the checkout-independent stable
  launcher, and the upgraded store retains v0.3.0 as a verified recovery
  version.

The smoke runs installer, bootstrap, launcher, lifecycle, and daemon commands
with a curated native-tool `PATH` that contains no Rust toolchain. Network
sentinels prove the current-version and downgrade paths complete without a
download client call, including after waiting on a live installation lock.

## Candidate and publication review

- [ ] Inspect the draft assets, `SHA256SUMS`, attestations, archive allowlist,
  compatibility metadata, and target metadata before publication.
- [ ] Confirm the lifecycle smoke passed on all four target runners. Do not
  substitute the Linux-only fresh-install container test for the native macOS
  gates.
- [ ] Confirm README, installation, remote-machine, troubleshooting, and
  architecture guidance matches the candidate behavior.
- [ ] Confirm TPM documentation keeps `prefix + U` checkout ownership separate
  from `tmux-agent update` binary ownership and describes `plugin update` as a
  deprecated compatibility command.
- [ ] Confirm remote updates are shown only as ordinary SSH commands initiated
  by the user.

After an authorized release is public, use a disposable previous-version
installation to run the anonymous public path once:

```sh
tmux-agent update
tmux-agent versions
tmux-agent rollback <previous-version>
tmux-agent update --version <released-version>
```

This post-public check validates GitHub's visible release metadata and asset
delivery. The draft gate cannot honestly prove anonymous discovery of assets
that GitHub has not published yet.
