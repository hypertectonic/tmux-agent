#!/usr/bin/env bash
set -euo pipefail

source_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent-package-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT

fixture="$test_root/source with spaces"
mkdir -p "$fixture/scripts" "$fixture/dist"
cp "$source_root/scripts/check-version" "$source_root/scripts/package-release" \
    "$fixture/scripts/"
chmod +x "$fixture/scripts/check-version" "$fixture/scripts/package-release"
printf '%s\n' 0.1.0 >"$fixture/VERSION"
cat >"$fixture/Cargo.toml" <<'EOF'
[package]
name = "tmux-agent"
version = "0.1.0"
EOF
cat >"$fixture/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "tmux-agent"
version = "0.1.0"
EOF
printf '%s\n' readme >"$fixture/README.md"
printf '%s\n' license >"$fixture/LICENSE"
printf '%s\n' notices >"$fixture/THIRD_PARTY_NOTICES.md"
printf '%s\n' licenses >"$fixture/THIRD_PARTY_LICENSES.html"
cat >"$fixture/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.0
EOF

make_binary() {
    local path=$1
    local version=$2
    cat >"$path" <<EOF
#!/bin/sh
printf '%s\\n' 'tmux-agent $version'
EOF
    chmod +x "$path"
}

binary="$test_root/tmux agent"
make_binary "$binary" 0.1.0
archive=$(
    "$fixture/scripts/package-release" \
        x86_64-unknown-linux-gnu "$binary" "$fixture/dist"
)
[[ $archive == "$fixture/dist/tmux-agent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" ]]
[[ -f $archive ]]

contents=$(tar -tzf "$archive" | sed 's#^\./##' | sort)
expected=$(
    printf '%s\n' COMPATIBILITY LICENSE README.md TARGET \
        THIRD_PARTY_LICENSES.html THIRD_PARTY_NOTICES.md tmux-agent | sort
)
[[ $contents == "$expected" ]]

extracted="$test_root/extracted"
mkdir "$extracted"
tar -xzf "$archive" -C "$extracted"
[[ $("$extracted/tmux-agent" --version) == 'tmux-agent 0.1.0' ]]
grep -F license "$extracted/LICENSE" >/dev/null
grep -F notices "$extracted/THIRD_PARTY_NOTICES.md" >/dev/null
grep -F licenses "$extracted/THIRD_PARTY_LICENSES.html" >/dev/null
grep -Fx 'launcher_protocol=1' "$extracted/COMPATIBILITY" >/dev/null
grep -Fx 'binary_version=0.1.0' "$extracted/COMPATIBILITY" >/dev/null
grep -Fx 'x86_64-unknown-linux-gnu' "$extracted/TARGET" >/dev/null

wrong_binary="$test_root/wrong version"
make_binary "$wrong_binary" 9.9.9
if "$fixture/scripts/package-release" \
    x86_64-unknown-linux-gnu "$wrong_binary" "$fixture/dist" \
    >"$test_root/wrong.out" 2>"$test_root/wrong.err"; then
    printf '%s\n' 'a mismatched binary version should fail packaging' >&2
    exit 1
fi
grep -F 'release binary version mismatch' "$test_root/wrong.err" >/dev/null

if "$fixture/scripts/package-release" \
    unsupported-target "$binary" "$fixture/dist" \
    >"$test_root/target.out" 2>"$test_root/target.err"; then
    printf '%s\n' 'an unsupported release target should fail packaging' >&2
    exit 1
fi
grep -F 'unsupported release target' "$test_root/target.err" >/dev/null

printf '%s\n' 'package release tests passed'
