#!/usr/bin/env bash
set -euo pipefail

source_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent-readiness-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT
fixture="$test_root/source"
mkdir -p \
    "$fixture/.github/ISSUE_TEMPLATE" \
    "$fixture/.github/workflows" \
    "$fixture/bin" \
    "$fixture/docs/assets" \
    "$fixture/docs" \
    "$fixture/scripts" \
    "$fixture/tests"

for file in \
    LICENSE \
    THIRD_PARTY_NOTICES.md \
    THIRD_PARTY_LICENSES.html \
    about.toml \
    about.hbs \
    CHANGELOG.md \
    CONTRIBUTING.md \
    SECURITY.md \
    VERSION \
    Cargo.lock \
    docs/installation.md \
    docs/remote-machines.md \
    docs/troubleshooting.md \
    docs/architecture.md \
    docs/dependency-licenses.md \
    docs/assets/ui-overview.svg \
    .github/ISSUE_TEMPLATE/feature.yml \
    .github/pull_request_template.md \
    .github/workflows/ci.yml; do
    printf '%s\n' fixture >"$fixture/$file"
done
cat >"$fixture/.github/ISSUE_TEMPLATE/bug.yml" <<'EOF'
description: Paste `tmux-agent doctor --json` after reviewing it. Do not paste terminal transcripts.
EOF
cat >"$fixture/.github/dependabot.yml" <<'EOF'
updates:
  - package-ecosystem: cargo
  - package-ecosystem: github-actions
EOF
printf '%s\n' '/.github/workflows/ @example-owner' \
    >"$fixture/.github/CODEOWNERS"
cat >"$fixture/.github/workflows/release.yml" <<'EOF'
permissions:
  contents: read
steps:
  - run: git merge-base --is-ancestor "$GITHUB_SHA" refs/remotes/origin/main
  - run: gh release create "$TAG" --draft --notes-file RELEASE_NOTES.md
EOF
cat >"$fixture/.github/workflows/ci.yml" <<'EOF'
permissions:
  contents: read
EOF
cat >"$fixture/Cargo.toml" <<'EOF'
[package]
name = "tmux-agent"
version = "0.1.0"
license = "MIT"
repository = "https://github.com/example-owner/tmux-agent"
homepage = "https://github.com/example-owner/tmux-agent"
readme = "README.md"
keywords = ["tmux"]
categories = ["command-line-utilities"]
EOF
cat >"$fixture/README.md" <<'EOF'
# tmux-agent

![interface](docs/assets/ui-overview.svg)

> **Early release:** Codex has received the most testing.

See [third-party notices](THIRD_PARTY_NOTICES.md).
See [dependency licenses](THIRD_PARTY_LICENSES.html).

## Install with a coding agent

Follow the deterministic installation procedure.

## License

Licensed under MIT.
EOF
printf '%s\n' 'tmux-agent vfixture' >"$fixture/RELEASE_NOTES.md"

cat >"$test_root/pass" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$test_root/pass"
for executable in \
    tmux-agent.tmux \
    bin/tmux-agent \
    scripts/bootstrap \
    scripts/install \
    scripts/launch-popup \
    scripts/lib.sh \
    scripts/doctor \
    scripts/uninstall \
    scripts/check-version \
    scripts/check-public-tree \
    scripts/generate-third-party-licenses \
    scripts/check-third-party-licenses \
    scripts/check-release-readiness \
    scripts/export-public-snapshot \
    scripts/package-release \
    tests/run-shell-tests; do
    cp "$test_root/pass" "$fixture/$executable"
done

"$source_root/scripts/check-release-readiness" "$fixture" >/dev/null

printf '%s\n' '<github-owner>' >>"$fixture/README.md"
if "$source_root/scripts/check-release-readiness" "$fixture" \
    >"$test_root/placeholder.out" 2>"$test_root/placeholder.err"; then
    printf '%s\n' 'owner placeholders should fail release readiness' >&2
    exit 1
fi
grep -F 'public owner placeholders remain' "$test_root/placeholder.err" >/dev/null

sed -i.bak '/^license = /d' "$fixture/Cargo.toml"
rm -f "$fixture/Cargo.toml.bak"
if "$source_root/scripts/check-release-readiness" "$fixture" \
    >"$test_root/license.out" 2>"$test_root/license.err"; then
    printf '%s\n' 'missing package license should fail readiness' >&2
    exit 1
fi
grep -F 'Cargo package license is missing' "$test_root/license.err" >/dev/null

export_source="$test_root/export-source"
mkdir -p "$export_source/scripts"
cp "$source_root/scripts/check-public-tree" "$export_source/scripts/"
cp "$source_root/scripts/export-public-snapshot" "$export_source/scripts/"
printf '%s\n' 'synthetic private marker' >"$export_source/fixture.txt"
git -C "$export_source" init -q
git -C "$export_source" add .
git -C "$export_source" \
    -c user.name=fixture \
    -c user.email=fixture@example.invalid \
    -c commit.gpgsign=false \
    commit -q -m fixture
printf '%s\t%s\n' 'exported synthetic marker found' 'private marker' \
    >"$export_source/.git/public-tree-denylist"
if "$export_source/scripts/export-public-snapshot" "$test_root/exported" \
    >"$test_root/export.out" 2>"$test_root/export.err"; then
    printf '%s\n' 'export should apply the source private denylist' >&2
    exit 1
fi
grep -F 'public-tree check failed: exported synthetic marker found' \
    "$test_root/export.err" >/dev/null

printf '%s\n' 'release readiness tests passed'
