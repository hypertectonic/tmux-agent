#!/usr/bin/env bash
set -euo pipefail

source_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TMUX_AGENT_ROOT=$source_root
export TMUX_AGENT_ROOT
# shellcheck source=scripts/lib.sh
. "$source_root/scripts/lib.sh"

[[ $(TMUX_AGENT_UNAME_S=Darwin TMUX_AGENT_UNAME_M=arm64 tmux_agent_target) == aarch64-apple-darwin ]]
[[ $(TMUX_AGENT_UNAME_S=Darwin TMUX_AGENT_UNAME_M=x86_64 tmux_agent_target) == x86_64-apple-darwin ]]
[[ $(TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=amd64 tmux_agent_target) == x86_64-unknown-linux-gnu ]]
[[ $(TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=aarch64 tmux_agent_target) == aarch64-unknown-linux-gnu ]]

test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent installer test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT
export TMUX_AGENT_UNAME_S=Linux
export TMUX_AGENT_UNAME_M=x86_64

plugin_root="$test_root/plugin with spaces"
release_root="$test_root/releases"
mkdir -p "$plugin_root/scripts" "$plugin_root/bin" "$release_root"
printf '%s\n' 0.1.0 >"$plugin_root/VERSION"
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.0
EOF
cp "$source_root/scripts/lib.sh" "$source_root/scripts/bootstrap" \
    "$source_root/scripts/install" "$source_root/scripts/standalone-launcher" \
    "$source_root/scripts/uninstall" "$plugin_root/scripts/"
cp "$source_root/bin/tmux-agent" "$plugin_root/bin/tmux-agent"
chmod +x "$plugin_root/scripts/"* "$plugin_root/bin/tmux-agent"

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

make_release() {
    local version=$1
    local reported_version=$2
    local release_dir="$release_root/v$version"
    local package_dir="$test_root/package-$version-$reported_version"
    local asset="tmux-agent-v${version}-x86_64-unknown-linux-gnu.tar.gz"
    rm -rf -- "$package_dir"
    mkdir -p "$release_dir" "$package_dir"
    cat >"$package_dir/tmux-agent" <<EOF
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then
    printf '%s\\n' 'tmux-agent $reported_version'
    exit 0
fi
if [ "\${1:-}" = "daemon" ] && [ "\${2:-}" = "restart" ]; then
    printf '%s\\n' 'tmux-agent $reported_version daemon restart' \
        >>"\${TMUX_AGENT_DAEMON_TEST_LOG:?}"
    exit 0
fi
command=\${1:-}
if [ "\$command" = --config ]; then
    shift 2
    command=\${1:-}
fi
case "\$command" in
    update | versions | rollback)
        if [ -n "\${TMUX_AGENT_LIFECYCLE_TEST_LOG:-}" ]; then
            printf '%s %s\\n' '$reported_version' "\$command" \
                >>"\$TMUX_AGENT_LIFECYCLE_TEST_LOG"
        fi
        ;;
esac
if [ "\$command" = update ]; then
    exit 0
fi
if [ "\$command" = versions ]; then
    current=\$(readlink "\${TMUX_AGENT_DATA_DIR:?}/current" 2>/dev/null || true)
    for directory in "\$TMUX_AGENT_DATA_DIR"/versions/*; do
        [ -d "\$directory" ] || continue
        installed=\${directory##*/}
        if [ "\$current" = "versions/\$installed/tmux-agent" ]; then
            printf 'active    %s\\n' "\$installed"
        else
            printf 'rollback  %s\\n' "\$installed"
        fi
    done
    exit 0
fi
if [ "\$command" = rollback ]; then
    requested=\${2:?}
    temporary="\${TMUX_AGENT_DATA_DIR:?}/.current.fake.\$\$"
    ln -s "versions/\$requested/tmux-agent" "\$temporary"
    mv -f "\$temporary" "\$TMUX_AGENT_DATA_DIR/current"
    exit 0
fi
exit 0
EOF
    chmod +x "$package_dir/tmux-agent"
    printf '%s\n' readme >"$package_dir/README.md"
    printf '%s\n' license >"$package_dir/LICENSE"
    printf '%s\n' notices >"$package_dir/THIRD_PARTY_NOTICES.md"
    printf '%s\n' licenses >"$package_dir/THIRD_PARTY_LICENSES.html"
    cat >"$package_dir/COMPATIBILITY" <<EOF
launcher_protocol=1
binary_version=$version
management_protocol=1
EOF
    printf '%s\n' x86_64-unknown-linux-gnu >"$package_dir/TARGET"
    tar -czf "$release_dir/$asset" -C "$package_dir" \
        tmux-agent README.md LICENSE THIRD_PARTY_NOTICES.md \
        THIRD_PARTY_LICENSES.html COMPATIBILITY TARGET
    printf '%s  %s\n' "$(checksum "$release_dir/$asset")" "$asset" \
        >"$release_dir/SHA256SUMS"
}

make_managed_version() {
    local data_dir=$1
    local version=$2
    local protocol=$3
    local management=${4:-1}
    local version_dir="$data_dir/versions/$version"
    mkdir -p "$version_dir"
    cat >"$version_dir/tmux-agent" <<EOF
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then
    printf '%s\\n' 'tmux-agent $version'
    exit 0
fi
exit 0
EOF
    chmod +x "$version_dir/tmux-agent"
    cat >"$version_dir/COMPATIBILITY" <<EOF
launcher_protocol=$protocol
binary_version=$version
EOF
    if [[ $management == 1 ]]; then
        printf '%s\n' 'management_protocol=1' >>"$version_dir/COMPATIBILITY"
    fi
    printf '%s\n' x86_64-unknown-linux-gnu >"$version_dir/TARGET"
}

make_current() {
    local data_dir=$1
    local version=$2
    mkdir -p "$data_dir"
    ln -s "versions/$version/tmux-agent" "$data_dir/current"
    if grep -Fxq 'launcher_protocol=1' "$data_dir/versions/$version/COMPATIBILITY" &&
        grep -Fxq 'management_protocol=1' "$data_dir/versions/$version/COMPATIBILITY"; then
        ln -s "versions/$version/tmux-agent" "$data_dir/manager"
    fi
}

make_legacy_tpm_current() {
    local data_dir=$1
    local version=$2
    make_managed_version "$data_dir" "$version" 1 0
    rm "$data_dir/versions/$version/COMPATIBILITY" \
        "$data_dir/versions/$version/TARGET"
    ln -s "versions/$version/tmux-agent" "$data_dir/current"
}

run_bootstrap() {
    local data_dir=$1
    local state_dir=$2
    shift 2
    TMUX_AGENT_DATA_DIR="$data_dir" \
        TMUX_AGENT_STATE_DIR="$state_dir" \
        TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
        TMUX_AGENT_UNAME_S=Linux \
        TMUX_AGENT_UNAME_M=x86_64 \
        "$plugin_root/scripts/bootstrap" --no-restart "$@"
}

make_release 0.1.0 0.1.0
data_dir="$test_root/data/tmux-agent"
state_dir="$test_root/state/tmux-agent"
secret='should-not-enter-install-logs'
TEST_SECRET="$secret" run_bootstrap "$data_dir" "$state_dir" \
    >"$test_root/install.log" 2>&1
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]
grep -Fx 'launcher_protocol=1' \
    "$data_dir/versions/0.1.0/COMPATIBILITY" >/dev/null
[[ -f $data_dir/versions/0.1.0/THIRD_PARTY_NOTICES.md ]]
[[ -f $data_dir/versions/0.1.0/THIRD_PARTY_LICENSES.html ]]
if grep -F "$secret" "$test_root/install.log"; then
    printf '%s\n' 'installer log exposed an unrelated environment secret' >&2
    exit 1
fi

mv "$release_root" "$test_root/releases-offline"
run_bootstrap "$data_dir" "$state_dir" >/dev/null
mv "$test_root/releases-offline" "$release_root"

below_data="$test_root/below-floor/tmux-agent"
make_managed_version "$below_data" 0.0.9 1
make_current "$below_data" 0.0.9
run_bootstrap "$below_data" "$test_root/below-floor-state/tmux-agent" >/dev/null
[[ $("$below_data/current" --version) == 'tmux-agent 0.1.0' ]]

newer_data="$test_root/newer-compatible/tmux-agent"
make_managed_version "$newer_data" 0.2.0 1
make_current "$newer_data" 0.2.0
mv "$release_root" "$test_root/releases-offline"
run_bootstrap "$newer_data" "$test_root/newer-state/tmux-agent" \
    >"$test_root/newer-bootstrap.out"
grep -F 'compatible managed binary 0.2.0 is already current' \
    "$test_root/newer-bootstrap.out" >/dev/null
TMUX_AGENT_DATA_DIR="$newer_data" \
    TMUX_AGENT_STATE_DIR="$test_root/newer-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/bin/tmux-agent" plugin update \
    >"$test_root/plugin-update.out" 2>"$test_root/plugin-update.err"
grep -F "'plugin update' is deprecated" "$test_root/plugin-update.err" >/dev/null
[[ $("$newer_data/current" --version) == 'tmux-agent 0.2.0' ]]
TMUX_AGENT_DATA_DIR="$newer_data" \
    TMUX_AGENT_STATE_DIR="$test_root/newer-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    "$plugin_root/bin/tmux-agent" --version \
    >"$test_root/newer-launcher.out"
grep -Fx 'tmux-agent 0.2.0' "$test_root/newer-launcher.out" >/dev/null
mv "$test_root/releases-offline" "$release_root"

plugin_update_data="$test_root/plugin-update-missing/tmux-agent"
TMUX_AGENT_DATA_DIR="$plugin_update_data" \
    TMUX_AGENT_STATE_DIR="$test_root/plugin-update-missing-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/bin/tmux-agent" plugin update \
    >"$test_root/plugin-update-missing.out" \
    2>"$test_root/plugin-update-missing.err"
grep -F "'plugin update' is deprecated" \
    "$test_root/plugin-update-missing.err" >/dev/null
[[ $("$plugin_update_data/current" --version) == 'tmux-agent 0.1.0' ]]

incompatible_data="$test_root/incompatible/tmux-agent"
make_managed_version "$incompatible_data" 0.2.0 2
make_current "$incompatible_data" 0.2.0
run_bootstrap "$incompatible_data" \
    "$test_root/incompatible-state/tmux-agent" >/dev/null
[[ $("$incompatible_data/current" --version) == 'tmux-agent 0.1.0' ]]

stale_data="$test_root/stale/tmux-agent"
mkdir -p "$stale_data/.install.lock"
printf '%s\n' 999999 >"$stale_data/.install.lock/pid"
run_bootstrap "$stale_data" "$test_root/stale-state/tmux-agent" >/dev/null
[[ $("$stale_data/current" --version) == 'tmux-agent 0.1.0' ]]

incomplete_lock_data="$test_root/incomplete-lock/tmux-agent"
mkdir -p "$incomplete_lock_data/.install.lock"
TMUX_AGENT_INCOMPLETE_LOCK_GRACE_ATTEMPTS=1 \
    run_bootstrap "$incomplete_lock_data" \
    "$test_root/incomplete-lock-state/tmux-agent" >/dev/null
[[ $("$incomplete_lock_data/current" --version) == 'tmux-agent 0.1.0' ]]

concurrent_data="$test_root/concurrent/tmux-agent"
run_bootstrap "$concurrent_data" "$test_root/concurrent-state/tmux-agent" >/dev/null &
first_pid=$!
run_bootstrap "$concurrent_data" "$test_root/concurrent-state/tmux-agent" >/dev/null &
second_pid=$!
wait "$first_pid"
wait "$second_pid"
[[ $("$concurrent_data/current" --version) == 'tmux-agent 0.1.0' ]]

bad_release_root="$test_root/bad-releases"
cp -R "$release_root" "$bad_release_root"
printf '%064d  %s\n' 0 tmux-agent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz \
    >"$bad_release_root/v0.1.0/SHA256SUMS"
bad_data="$test_root/bad/tmux-agent"
if TMUX_AGENT_DATA_DIR="$bad_data" \
    TMUX_AGENT_STATE_DIR="$test_root/bad-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${bad_release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/bootstrap" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'invalid checksum should fail' >&2
    exit 1
fi
[[ ! -e $bad_data/current ]]
grep -F 'checksum mismatch' "$bad_data/install-status" >/dev/null
run_bootstrap "$bad_data" "$test_root/bad-state/tmux-agent" >/dev/null
[[ $("$bad_data/current" --version) == 'tmux-agent 0.1.0' ]]

interrupted_release_root="$test_root/interrupted-releases"
cp -R "$release_root" "$interrupted_release_root"
interrupted_asset="$interrupted_release_root/v0.1.0/tmux-agent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
printf '%s\n' 'partial archive' >"$interrupted_asset"
printf '%s  %s\n' "$(checksum "$interrupted_asset")" \
    tmux-agent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz \
    >"$interrupted_release_root/v0.1.0/SHA256SUMS"
interrupted_data="$test_root/interrupted/tmux-agent"
if TMUX_AGENT_DATA_DIR="$interrupted_data" \
    TMUX_AGENT_STATE_DIR="$test_root/interrupted-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${interrupted_release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/bootstrap" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'a partial archive should fail' >&2
    exit 1
fi
[[ ! -e $interrupted_data/current ]]
cp "$release_root/v0.1.0/"* "$interrupted_release_root/v0.1.0/"
TMUX_AGENT_DATA_DIR="$interrupted_data" \
    TMUX_AGENT_STATE_DIR="$test_root/interrupted-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${interrupted_release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/bootstrap" --no-restart >/dev/null
[[ $("$interrupted_data/current" --version) == 'tmux-agent 0.1.0' ]]

unsupported_data="$test_root/unsupported/tmux-agent"
if TMUX_AGENT_DATA_DIR="$unsupported_data" \
    TMUX_AGENT_STATE_DIR="$test_root/unsupported-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Plan9 TMUX_AGENT_UNAME_M=mips \
    "$plugin_root/scripts/bootstrap" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'unsupported platform should fail before download' >&2
    exit 1
fi
grep -F 'UNSUPPORTED|Plan9/mips' "$unsupported_data/install-status" >/dev/null

unsafe_release_root="$test_root/unsafe-releases"
unsafe_package="$test_root/unsafe-package"
unsafe_asset=tmux-agent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$unsafe_release_root/v0.1.0" "$unsafe_package"
cp "$test_root/package-0.1.0-0.1.0/tmux-agent" "$unsafe_package/tmux-agent"
printf '%s\n' readme >"$unsafe_package/README.md"
printf '%s\n' notices >"$unsafe_package/THIRD_PARTY_NOTICES.md"
printf '%s\n' licenses >"$unsafe_package/THIRD_PARTY_LICENSES.html"
cat >"$unsafe_package/COMPATIBILITY" <<'EOF'
launcher_protocol=1
binary_version=0.1.0
management_protocol=1
EOF
printf '%s\n' x86_64-unknown-linux-gnu >"$unsafe_package/TARGET"
ln -s /etc/passwd "$unsafe_package/LICENSE"
tar -czf "$unsafe_release_root/v0.1.0/$unsafe_asset" -C "$unsafe_package" \
    tmux-agent README.md LICENSE THIRD_PARTY_NOTICES.md \
    THIRD_PARTY_LICENSES.html COMPATIBILITY TARGET
printf '%s  %s\n' \
    "$(checksum "$unsafe_release_root/v0.1.0/$unsafe_asset")" "$unsafe_asset" \
    >"$unsafe_release_root/v0.1.0/SHA256SUMS"
unsafe_data="$test_root/unsafe/tmux-agent"
if TMUX_AGENT_DATA_DIR="$unsafe_data" \
    TMUX_AGENT_STATE_DIR="$test_root/unsafe-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${unsafe_release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/bootstrap" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'archive symlinks should be rejected' >&2
    exit 1
fi
grep -F 'non-regular entry' "$unsafe_data/install-status" >/dev/null

printf '%s\n' 0.1.1 >"$plugin_root/VERSION"
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.1
EOF
make_release 0.1.1 9.9.9
if run_bootstrap "$data_dir" "$state_dir" >/dev/null 2>&1; then
    printf '%s\n' 'wrong binary version should fail' >&2
    exit 1
fi
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]

make_release 0.1.1 0.1.1
daemon_test_log="$test_root/daemon-restarts.log"
TMUX_AGENT_DAEMON_TEST_LOG="$daemon_test_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" \
    TMUX_AGENT_STATE_DIR="$state_dir" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/bootstrap" >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.1' ]]
grep -F 'tmux-agent 0.1.1 daemon restart' "$daemon_test_log" >/dev/null
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.0
EOF

legacy_tpm_data="$test_root/legacy-tpm/tmux-agent"
legacy_tpm_state="$test_root/legacy-tpm-state/tmux-agent"
make_legacy_tpm_current "$legacy_tpm_data" 0.1.0
run_bootstrap "$legacy_tpm_data" "$legacy_tpm_state" >/dev/null
[[ $(readlink "$legacy_tpm_data/current") == 'versions/0.1.0/tmux-agent' ]]
[[ $(readlink "$legacy_tpm_data/manager") == 'versions/0.1.1/tmux-agent' ]]
[[ $(cat "$legacy_tpm_data/versions/0.1.0/TARGET") == x86_64-unknown-linux-gnu ]]
grep -Fx 'launcher_protocol=1' \
    "$legacy_tpm_data/versions/0.1.0/COMPATIBILITY" >/dev/null
grep -Fx 'binary_version=0.1.0' \
    "$legacy_tpm_data/versions/0.1.0/COMPATIBILITY" >/dev/null
if grep -q '^management_protocol=' \
    "$legacy_tpm_data/versions/0.1.0/COMPATIBILITY"; then
    printf '%s\n' 'legacy TPM migration granted lifecycle-controller capability' >&2
    exit 1
fi
legacy_versions=$(TMUX_AGENT_DATA_DIR="$legacy_tpm_data" \
    "$plugin_root/bin/tmux-agent" versions)
[[ $legacy_versions == *'0.1.0'* && $legacy_versions == *'0.1.1'* ]]
TMUX_AGENT_DATA_DIR="$legacy_tpm_data" TMUX_AGENT_STATE_DIR="$legacy_tpm_state" \
    "$plugin_root/bin/tmux-agent" rollback 0.1.1 >/dev/null
[[ $(readlink "$legacy_tpm_data/current") == 'versions/0.1.1/tmux-agent' ]]
[[ $(readlink "$legacy_tpm_data/manager") == 'versions/0.1.1/tmux-agent' ]]
TMUX_AGENT_DATA_DIR="$legacy_tpm_data" TMUX_AGENT_STATE_DIR="$legacy_tpm_state" \
    "$plugin_root/bin/tmux-agent" rollback 0.1.0 >/dev/null
[[ $(readlink "$legacy_tpm_data/current") == 'versions/0.1.0/tmux-agent' ]]
[[ $(readlink "$legacy_tpm_data/manager") == 'versions/0.1.1/tmux-agent' ]]

legacy_resume_data="$test_root/legacy-resume/tmux-agent"
make_legacy_tpm_current "$legacy_resume_data" 0.1.0
printf '%s\n' x86_64-unknown-linux-gnu \
    >"$legacy_resume_data/versions/0.1.0/TARGET"
run_bootstrap "$legacy_resume_data" \
    "$test_root/legacy-resume-state/tmux-agent" >/dev/null
grep -Fx 'binary_version=0.1.0' \
    "$legacy_resume_data/versions/0.1.0/COMPATIBILITY" >/dev/null

assert_legacy_tpm_rejected() {
    local description=$1
    local data_dir=$2
    local state_dir=$3
    local original_current
    original_current=$(readlink "$data_dir/current")
    if run_bootstrap "$data_dir" "$state_dir" >/dev/null 2>&1; then
        printf 'legacy TPM migration accepted %s\n' "$description" >&2
        exit 1
    fi
    [[ $(readlink "$data_dir/current") == "$original_current" ]]
    [[ ! -e $data_dir/manager && ! -L $data_dir/manager ]]
}

legacy_partial_data="$test_root/legacy-partial/tmux-agent"
make_legacy_tpm_current "$legacy_partial_data" 0.1.0
cat >"$legacy_partial_data/versions/0.1.0/COMPATIBILITY" <<'EOF'
launcher_protocol=1
binary_version=0.1.0
EOF
assert_legacy_tpm_rejected 'COMPATIBILITY-only partial metadata' \
    "$legacy_partial_data" "$test_root/legacy-partial-state/tmux-agent"
[[ ! -e $legacy_partial_data/versions/0.1.0/TARGET ]]

legacy_wrong_target_data="$test_root/legacy-wrong-target/tmux-agent"
make_legacy_tpm_current "$legacy_wrong_target_data" 0.1.0
printf '%s\n' aarch64-unknown-linux-gnu \
    >"$legacy_wrong_target_data/versions/0.1.0/TARGET"
assert_legacy_tpm_rejected 'mismatched resumable target metadata' \
    "$legacy_wrong_target_data" \
    "$test_root/legacy-wrong-target-state/tmux-agent"
[[ ! -e $legacy_wrong_target_data/versions/0.1.0/COMPATIBILITY ]]

legacy_below_floor_data="$test_root/legacy-below-floor/tmux-agent"
make_legacy_tpm_current "$legacy_below_floor_data" 0.0.9
assert_legacy_tpm_rejected 'a binary below the checkout compatibility floor' \
    "$legacy_below_floor_data" \
    "$test_root/legacy-below-floor-state/tmux-agent"
[[ ! -e $legacy_below_floor_data/versions/0.0.9/TARGET ]]

legacy_newer_data="$test_root/legacy-newer/tmux-agent"
make_legacy_tpm_current "$legacy_newer_data" 0.2.0
printf '%s\n' 0.1.0 >"$plugin_root/VERSION"
assert_legacy_tpm_rejected 'a legacy binary newer than the checkout candidate' \
    "$legacy_newer_data" "$test_root/legacy-newer-state/tmux-agent"
[[ ! -e $legacy_newer_data/versions/0.2.0/TARGET ]]
[[ ! -e $legacy_newer_data/versions/0.2.0/COMPATIBILITY ]]
printf '%s\n' 0.1.1 >"$plugin_root/VERSION"

legacy_equal_precedence_data="$test_root/legacy-equal-precedence/tmux-agent"
make_legacy_tpm_current "$legacy_equal_precedence_data" 0.2.0
printf '%s\n' 0.2.0+candidate >"$plugin_root/VERSION"
assert_legacy_tpm_rejected 'equal-precedence build metadata' \
    "$legacy_equal_precedence_data" \
    "$test_root/legacy-equal-precedence-state/tmux-agent"
[[ ! -e $legacy_equal_precedence_data/versions/0.2.0/TARGET ]]
[[ ! -e $legacy_equal_precedence_data/versions/0.2.0/COMPATIBILITY ]]
printf '%s\n' 0.1.1 >"$plugin_root/VERSION"

legacy_candidate_floor_data="$test_root/legacy-candidate-floor/tmux-agent"
make_legacy_tpm_current "$legacy_candidate_floor_data" 0.2.0
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.2.0
EOF
assert_legacy_tpm_rejected 'a checkout below its own compatibility floor' \
    "$legacy_candidate_floor_data" \
    "$test_root/legacy-candidate-floor-state/tmux-agent"
[[ ! -e $legacy_candidate_floor_data/versions/0.2.0/TARGET ]]
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.0
EOF

legacy_manager_data="$test_root/legacy-manager/tmux-agent"
make_legacy_tpm_current "$legacy_manager_data" 0.1.0
ln -s versions/0.1.0/tmux-agent "$legacy_manager_data/manager"
if run_bootstrap "$legacy_manager_data" \
    "$test_root/legacy-manager-state/tmux-agent" >/dev/null 2>&1; then
    printf '%s\n' 'legacy TPM migration accepted an existing manager selection' >&2
    exit 1
fi
[[ ! -e $legacy_manager_data/versions/0.1.0/TARGET ]]

legacy_outside_data="$test_root/legacy-outside/tmux-agent"
legacy_outside_version="$test_root/legacy-outside-version/versions/0.1.0"
make_managed_version "$test_root/legacy-outside-version" 0.1.0 1 0
rm "$legacy_outside_version/COMPATIBILITY" "$legacy_outside_version/TARGET"
mkdir -p "$legacy_outside_data"
ln -s "$legacy_outside_version/tmux-agent" "$legacy_outside_data/current"
assert_legacy_tpm_rejected 'an out-of-store current target' \
    "$legacy_outside_data" "$test_root/legacy-outside-state/tmux-agent"

legacy_symlink_data="$test_root/legacy-symlink/tmux-agent"
legacy_symlink_external="$test_root/legacy-symlink-external/tmux-agent"
make_legacy_tpm_current "$legacy_symlink_external" 0.1.0
mkdir -p "$legacy_symlink_data/versions"
ln -s "$legacy_symlink_external/versions/0.1.0" \
    "$legacy_symlink_data/versions/0.1.0"
ln -s versions/0.1.0/tmux-agent "$legacy_symlink_data/current"
assert_legacy_tpm_rejected 'a symlinked version directory' \
    "$legacy_symlink_data" "$test_root/legacy-symlink-state/tmux-agent"

TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" plugin rollback 0.1.0 >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]
versions=$(TMUX_AGENT_DATA_DIR="$data_dir" \
    "$plugin_root/bin/tmux-agent" plugin versions)
[[ $versions == *'0.1.0'* && $versions == *'0.1.1'* ]]
[[ $(readlink "$data_dir/manager") == 'versions/0.1.1/tmux-agent' ]]

make_managed_version "$data_dir" 0.0.9 1 0
lifecycle_log="$test_root/lifecycle-controller.log"
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" plugin rollback 0.0.9 >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.0.9' ]]
[[ $(readlink "$data_dir/manager") == 'versions/0.1.1/tmux-agent' ]]
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" plugin versions >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" --config "$test_root/config.toml" versions >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" plugin rollback 0.1.0 >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" update >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]

standalone_test_launcher="$test_root/standalone-controller"
cp "$source_root/scripts/standalone-launcher" "$standalone_test_launcher"
chmod +x "$standalone_test_launcher"
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" rollback 0.0.9 >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.0.9' ]]
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" --config "$test_root/config.toml" versions >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" rollback 0.1.0 >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" update >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin rollback 0.0.9 >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.0.9' ]]
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin versions >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin rollback 0.1.0 >/dev/null
TMUX_AGENT_LIFECYCLE_TEST_LOG="$lifecycle_log" TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin update \
    >/dev/null 2>"$test_root/standalone-plugin-update.err"
grep -F "running standalone update" \
    "$test_root/standalone-plugin-update.err" >/dev/null
if TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin versions extra >/dev/null 2>&1; then
    printf '%s\n' 'standalone launcher accepted extra plugin versions arguments' >&2
    exit 1
fi
if TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin rollback >/dev/null 2>&1; then
    printf '%s\n' 'standalone launcher accepted a missing rollback version' >&2
    exit 1
fi
if TMUX_AGENT_DATA_DIR="$data_dir" \
    "$standalone_test_launcher" plugin update extra >/dev/null 2>&1; then
    printf '%s\n' 'standalone launcher accepted extra plugin update arguments' >&2
    exit 1
fi
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]
if grep -vE '^0\.1\.1 (rollback|versions|update)$' "$lifecycle_log"; then
    printf '%s\n' 'a legacy current binary handled a lifecycle command' >&2
    exit 1
fi
[[ $(grep -c '^0\.1\.1 rollback$' "$lifecycle_log") -ge 4 ]]
[[ $(grep -c '^0\.1\.1 versions$' "$lifecycle_log") -ge 3 ]]
[[ $(grep -c '^0\.1\.1 update$' "$lifecycle_log") -ge 2 ]]

monotonic_data="$test_root/monotonic-manager/tmux-agent"
make_managed_version "$monotonic_data" 0.0.9 1 0
make_managed_version "$monotonic_data" 0.2.0 1
ln -s versions/0.0.9/tmux-agent "$monotonic_data/current"
ln -s versions/0.2.0/tmux-agent "$monotonic_data/manager"
run_bootstrap "$monotonic_data" \
    "$test_root/monotonic-manager-state/tmux-agent" >/dev/null
[[ $("$monotonic_data/current" --version) == 'tmux-agent 0.1.1' ]]
[[ $(readlink "$monotonic_data/manager") == 'versions/0.2.0/tmux-agent' ]]

rm "$data_dir/manager"
ln -s versions/0.0.9/tmux-agent "$data_dir/manager"
if TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" versions >/dev/null 2>&1; then
    printf '%s\n' 'TPM launcher accepted a legacy lifecycle controller' >&2
    exit 1
fi
if TMUX_AGENT_DATA_DIR="$data_dir" "$standalone_test_launcher" versions \
    >/dev/null 2>&1; then
    printf '%s\n' 'standalone launcher accepted a legacy lifecycle controller' >&2
    exit 1
fi
rm "$data_dir/manager"
ln -s versions/0.1.1/tmux-agent "$data_dir/manager"

assert_lifecycle_launchers_reject_manager() {
    local description=$1
    local marker=$2
    rm -f -- "$marker"
    if TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
        "$plugin_root/bin/tmux-agent" versions >/dev/null 2>&1; then
        printf 'TPM launcher accepted %s\n' "$description" >&2
        exit 1
    fi
    if TMUX_AGENT_DATA_DIR="$data_dir" "$standalone_test_launcher" versions \
        >/dev/null 2>&1; then
        printf 'standalone launcher accepted %s\n' "$description" >&2
        exit 1
    fi
    if [[ -e $marker ]]; then
        printf 'a launcher executed %s before validating it\n' "$description" >&2
        exit 1
    fi
}

malicious_marker="$test_root/malicious-manager-executed"
cat >"$data_dir/tmux-agent" <<EOF
#!/bin/sh
: >'$malicious_marker'
printf '%s\n' 'tmux-agent ..'
EOF
chmod +x "$data_dir/tmux-agent"
rm "$data_dir/manager"
ln -s versions/../tmux-agent "$data_dir/manager"
assert_lifecycle_launchers_reject_manager \
    'a traversal lifecycle-controller target' "$malicious_marker"

invalid_version_dir="$data_dir/versions/1.2"
mkdir "$invalid_version_dir"
cat >"$invalid_version_dir/tmux-agent" <<EOF
#!/bin/sh
: >'$malicious_marker'
printf '%s\n' 'tmux-agent 1.2'
EOF
chmod +x "$invalid_version_dir/tmux-agent"
cat >"$invalid_version_dir/COMPATIBILITY" <<'EOF'
launcher_protocol=1
binary_version=1.2
management_protocol=1
EOF
printf '%s\n' x86_64-unknown-linux-gnu >"$invalid_version_dir/TARGET"
rm "$data_dir/manager"
ln -s versions/1.2/tmux-agent "$data_dir/manager"
assert_lifecycle_launchers_reject_manager \
    'a non-SemVer lifecycle-controller target' "$malicious_marker"

escaped_manager_dir="$test_root/escaped-manager/versions/1.2.3"
mkdir -p "$escaped_manager_dir"
cat >"$escaped_manager_dir/tmux-agent" <<EOF
#!/bin/sh
: >'$malicious_marker'
printf '%s\n' 'tmux-agent 1.2.3'
EOF
chmod +x "$escaped_manager_dir/tmux-agent"
cat >"$escaped_manager_dir/COMPATIBILITY" <<'EOF'
launcher_protocol=1
binary_version=1.2.3
management_protocol=1
EOF
printf '%s\n' x86_64-unknown-linux-gnu >"$escaped_manager_dir/TARGET"
rm "$data_dir/manager"
ln -s "$escaped_manager_dir/tmux-agent" "$data_dir/manager"
assert_lifecycle_launchers_reject_manager \
    'an out-of-store lifecycle-controller target' "$malicious_marker"

symlinked_version_dir="$test_root/symlinked-version/1.2.3"
mkdir -p "$symlinked_version_dir"
cp "$escaped_manager_dir/tmux-agent" "$symlinked_version_dir/tmux-agent"
cp "$escaped_manager_dir/COMPATIBILITY" "$symlinked_version_dir/COMPATIBILITY"
cp "$escaped_manager_dir/TARGET" "$symlinked_version_dir/TARGET"
ln -s "$symlinked_version_dir" "$data_dir/versions/1.2.3"
rm "$data_dir/manager"
ln -s versions/1.2.3/tmux-agent "$data_dir/manager"
assert_lifecycle_launchers_reject_manager \
    'a lifecycle controller in a symlinked version directory' "$malicious_marker"

rm "$data_dir/manager"
ln -s versions/0.1.1/tmux-agent "$data_dir/manager"

direct_home="$test_root/direct-home"
direct_path="$direct_home/.local/bin/tmux-agent"
direct_data="$test_root/direct-data/tmux-agent"
mkdir -p "${direct_path%/*}"
cat >"$direct_path" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --version ]; then
    printf '%s\n' 'tmux-agent 0.0.9'
    exit 0
fi
exit 0
EOF
chmod +x "$direct_path"
HOME="$direct_home" TMUX_AGENT_INSTALL_PATH="$direct_path" \
    TMUX_AGENT_DATA_DIR="$direct_data" \
    TMUX_AGENT_STATE_DIR="$test_root/direct-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >"$test_root/direct-install.out"
grep -Fq '# tmux-agent managed launcher' "$direct_path"
[[ $("$direct_data/versions/0.0.9/tmux-agent" --version) == 'tmux-agent 0.0.9' ]]
[[ $("$direct_data/current" --version) == 'tmux-agent 0.1.1' ]]
[[ $(cat "$direct_data/versions/0.0.9/TARGET") == x86_64-unknown-linux-gnu ]]
HOME="$direct_home" TMUX_AGENT_INSTALL_PATH="$direct_path" \
    TMUX_AGENT_DATA_DIR="$direct_data" \
    TMUX_AGENT_STATE_DIR="$test_root/direct-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >/dev/null
[[ $("$direct_data/versions/0.0.9/tmux-agent" --version) == 'tmux-agent 0.0.9' ]]

failed_direct_home="$test_root/failed-direct-home"
failed_direct_path="$failed_direct_home/.local/bin/tmux-agent"
missing_release_root="$test_root/missing-releases"
mkdir -p "${failed_direct_path%/*}"
mkdir -p "$missing_release_root"
cp "$direct_data/versions/0.0.9/tmux-agent" "$failed_direct_path"
if HOME="$failed_direct_home" TMUX_AGENT_INSTALL_PATH="$failed_direct_path" \
    TMUX_AGENT_DATA_DIR="$test_root/failed-direct-data/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/failed-direct-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${missing_release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'failed migration bootstrap should fail' >&2
    exit 1
fi
[[ $("$failed_direct_path" --version) == 'tmux-agent 0.0.9' ]]
if grep -Fq '# tmux-agent managed launcher' "$failed_direct_path"; then
    printf '%s\n' 'failed migration replaced the direct binary' >&2
    exit 1
fi

same_version_home="$test_root/same-version-home"
same_version_path="$same_version_home/.local/bin/tmux-agent"
same_version_data="$test_root/same-version-data/tmux-agent"
same_version_original="$test_root/same-version-original"
mkdir -p "${same_version_path%/*}"
cp "$test_root/package-0.1.1-0.1.1/tmux-agent" "$same_version_path"
cp "$same_version_path" "$same_version_original"
HOME="$same_version_home" TMUX_AGENT_INSTALL_PATH="$same_version_path" \
    TMUX_AGENT_DATA_DIR="$same_version_data" \
    TMUX_AGENT_STATE_DIR="$test_root/same-version-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >/dev/null
cmp -s "$same_version_original" \
    "$same_version_data/versions/0.1.1/tmux-agent"
[[ $("$same_version_data/current" --version) == 'tmux-agent 0.1.1' ]]
[[ $(readlink "$same_version_data/manager") == 'versions/0.1.1/tmux-agent' ]]

printf '%s\n' '# older official launcher body' >>"$same_version_path"
if cmp -s "$same_version_path" "$plugin_root/scripts/standalone-launcher"; then
    printf '%s\n' 'older launcher fixture unexpectedly matches the new launcher' >&2
    exit 1
fi
HOME="$same_version_home" TMUX_AGENT_INSTALL_PATH="$same_version_path" \
    TMUX_AGENT_DATA_DIR="$same_version_data" \
    TMUX_AGENT_STATE_DIR="$test_root/same-version-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >/dev/null
cmp -s "$same_version_path" "$plugin_root/scripts/standalone-launcher"

custom_same_home="$test_root/custom-same-home"
custom_same_path="$custom_same_home/.local/bin/tmux-agent"
custom_same_original="$test_root/custom-same-original"
custom_same_data="$test_root/custom-same-data/tmux-agent"
mkdir -p "${custom_same_path%/*}"
cat >"$custom_same_path" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --version ]; then
    printf '%s\n' 'tmux-agent 0.1.1'
    exit 0
fi
exit 0
EOF
chmod +x "$custom_same_path"
cp "$custom_same_path" "$custom_same_original"
if HOME="$custom_same_home" TMUX_AGENT_INSTALL_PATH="$custom_same_path" \
    TMUX_AGENT_DATA_DIR="$custom_same_data" \
    TMUX_AGENT_STATE_DIR="$test_root/custom-same-state/tmux-agent" \
    TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
    TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
    "$plugin_root/scripts/install" --no-restart >/dev/null 2>&1; then
    printf '%s\n' 'custom same-version direct binary should fail closed' >&2
    exit 1
fi
cmp -s "$custom_same_path" "$custom_same_original"
[[ ! -e $custom_same_data/current && ! -e $custom_same_data/manager ]]

make_collision_direct() {
    local collision_path=$1
    mkdir -p "${collision_path%/*}"
    cp "$direct_data/versions/0.0.9/tmux-agent" "$collision_path"
}

make_collision_version() {
    local collision_data=$1
    make_managed_version "$collision_data" 0.0.9 1 0
}

assert_migration_collision_rejected() {
    local description=$1
    local collision_path=$2
    local collision_data=$3
    local original="$collision_path.original"
    cp "$collision_path" "$original"
    ln -s keep-current "$collision_data/current"
    if TMUX_AGENT_INSTALL_PATH="$collision_path" \
        TMUX_AGENT_DATA_DIR="$collision_data" \
        TMUX_AGENT_STATE_DIR="$collision_data-state" \
        TMUX_AGENT_RELEASE_BASE_URL="file://${release_root// /%20}" \
        TMUX_AGENT_UNAME_S=Linux TMUX_AGENT_UNAME_M=x86_64 \
        "$plugin_root/scripts/install" --no-restart >/dev/null 2>&1; then
        printf 'migration accepted %s\n' "$description" >&2
        exit 1
    fi
    cmp -s "$collision_path" "$original"
    [[ $(readlink "$collision_data/current") == keep-current ]]
}

collision_root="$test_root/migration-collisions"
versions_link_path="$collision_root/versions-link/bin/tmux-agent"
versions_link_data="$collision_root/versions-link/data/tmux-agent"
make_collision_direct "$versions_link_path"
mkdir -p "$versions_link_data" "$collision_root/external-versions"
ln -s "$collision_root/external-versions" "$versions_link_data/versions"
assert_migration_collision_rejected 'a symlinked versions store' \
    "$versions_link_path" "$versions_link_data"

version_link_path="$collision_root/version-link/bin/tmux-agent"
version_link_data="$collision_root/version-link/data/tmux-agent"
external_version_data="$collision_root/version-link/external/tmux-agent"
make_collision_direct "$version_link_path"
make_collision_version "$external_version_data"
mkdir -p "$version_link_data/versions"
ln -s "$external_version_data/versions/0.0.9" \
    "$version_link_data/versions/0.0.9"
assert_migration_collision_rejected 'a symlinked version directory' \
    "$version_link_path" "$version_link_data"

for collision_file in tmux-agent COMPATIBILITY TARGET; do
    collision_name=${collision_file//-/_}
    file_link_path="$collision_root/$collision_name-link/bin/tmux-agent"
    file_link_data="$collision_root/$collision_name-link/data/tmux-agent"
    make_collision_direct "$file_link_path"
    make_collision_version "$file_link_data"
    external_file="$collision_root/$collision_name-link/external-$collision_name"
    cp "$file_link_data/versions/0.0.9/$collision_file" "$external_file"
    rm "$file_link_data/versions/0.0.9/$collision_file"
    ln -s "$external_file" "$file_link_data/versions/0.0.9/$collision_file"
    assert_migration_collision_rejected "a symlinked $collision_file collision" \
        "$file_link_path" "$file_link_data"
done

unmarked_data="$test_root/unmarked/tmux-agent"
mkdir -p "$unmarked_data"
printf '%s\n' keep >"$unmarked_data/user-file"
if TMUX_AGENT_DATA_DIR="$unmarked_data" \
    TMUX_AGENT_STATE_DIR="$test_root/unmarked-state/tmux-agent" \
    "$plugin_root/scripts/uninstall" \
    >"$test_root/unmarked.out" 2>"$test_root/unmarked.err"; then
    printf '%s\n' 'uninstall should refuse an unmarked data directory' >&2
    exit 1
fi
grep -F 'refusing to remove unmarked directory' \
    "$test_root/unmarked.err" >/dev/null
[[ -f $unmarked_data/user-file ]]

home_dir="$test_root/home"
config_dir="$home_dir/config/tmux-agent"
mkdir -p "$config_dir" "$state_dir"
printf '%s\n' keep >"$config_dir/config.toml"
printf '%s\n' keep >"$state_dir/manual.log"
HOME="$home_dir" XDG_CONFIG_HOME="$home_dir/config" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/scripts/uninstall" >/dev/null
[[ ! -e $data_dir ]]
[[ -f $config_dir/config.toml ]]
[[ -f $state_dir/manual.log ]]

run_bootstrap "$data_dir" "$state_dir" >/dev/null
HOME="$home_dir" XDG_CONFIG_HOME="$home_dir/config" \
    TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/scripts/uninstall" --purge >/dev/null
[[ ! -e $data_dir && ! -e $config_dir && ! -e $state_dir ]]

managed_uninstall_path="$test_root/uninstall-launchers/managed/tmux-agent"
mkdir -p "${managed_uninstall_path%/*}"
cp "$plugin_root/scripts/standalone-launcher" "$managed_uninstall_path"
TMUX_AGENT_INSTALL_PATH="$managed_uninstall_path" \
    TMUX_AGENT_DATA_DIR="$test_root/uninstall-launchers/managed-data/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/uninstall-launchers/managed-state/tmux-agent" \
    "$plugin_root/scripts/uninstall" >/dev/null
[[ ! -e $managed_uninstall_path ]]

unrelated_uninstall_path="$test_root/uninstall-launchers/unrelated/tmux-agent"
mkdir -p "${unrelated_uninstall_path%/*}"
cat >"$unrelated_uninstall_path" <<'EOF'
#!/bin/sh
# tmux-agent managed launcher
# not-the-launcher-protocol
# tmux-agent-standalone-launcher-protocol=1
exit 0
EOF
chmod +x "$unrelated_uninstall_path"
TMUX_AGENT_INSTALL_PATH="$unrelated_uninstall_path" \
    TMUX_AGENT_DATA_DIR="$test_root/uninstall-launchers/unrelated-data/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/uninstall-launchers/unrelated-state/tmux-agent" \
    "$plugin_root/scripts/uninstall" >/dev/null
[[ -x $unrelated_uninstall_path ]]

symlink_uninstall_target="$test_root/uninstall-launchers/symlink-target"
symlink_uninstall_path="$test_root/uninstall-launchers/symlink/tmux-agent"
printf '%s\n' keep >"$symlink_uninstall_target"
mkdir -p "${symlink_uninstall_path%/*}"
ln -s "$symlink_uninstall_target" "$symlink_uninstall_path"
TMUX_AGENT_INSTALL_PATH="$symlink_uninstall_path" \
    TMUX_AGENT_DATA_DIR="$test_root/uninstall-launchers/symlink-data/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/uninstall-launchers/symlink-state/tmux-agent" \
    "$plugin_root/scripts/uninstall" >/dev/null
[[ -L $symlink_uninstall_path && -f $symlink_uninstall_target ]]

printf '%s\n' 'installer integration tests passed'
