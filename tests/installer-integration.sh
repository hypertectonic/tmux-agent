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

plugin_root="$test_root/plugin with spaces"
release_root="$test_root/releases"
mkdir -p "$plugin_root/scripts" "$plugin_root/bin" "$release_root"
printf '%s\n' 0.1.0 >"$plugin_root/VERSION"
cat >"$plugin_root/COMPATIBILITY" <<'EOF'
launcher_protocol=1
minimum_binary_version=0.1.0
EOF
cp "$source_root/scripts/lib.sh" "$source_root/scripts/bootstrap" \
    "$source_root/scripts/install" "$source_root/scripts/uninstall" "$plugin_root/scripts/"
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
exit 0
EOF
    chmod +x "$package_dir/tmux-agent"
    printf '%s\n' readme >"$package_dir/README.md"
    printf '%s\n' license >"$package_dir/LICENSE"
    printf '%s\n' notices >"$package_dir/THIRD_PARTY_NOTICES.md"
    printf '%s\n' licenses >"$package_dir/THIRD_PARTY_LICENSES.html"
    tar -czf "$release_dir/$asset" -C "$package_dir" \
        tmux-agent README.md LICENSE THIRD_PARTY_NOTICES.md \
        THIRD_PARTY_LICENSES.html
    printf '%s  %s\n' "$(checksum "$release_dir/$asset")" "$asset" \
        >"$release_dir/SHA256SUMS"
}

make_managed_version() {
    local data_dir=$1
    local version=$2
    local protocol=$3
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
}

make_current() {
    local data_dir=$1
    local version=$2
    mkdir -p "$data_dir"
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
ln -s /etc/passwd "$unsafe_package/LICENSE"
tar -czf "$unsafe_release_root/v0.1.0/$unsafe_asset" -C "$unsafe_package" \
    tmux-agent README.md LICENSE THIRD_PARTY_NOTICES.md \
    THIRD_PARTY_LICENSES.html
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
TMUX_AGENT_DATA_DIR="$data_dir" TMUX_AGENT_STATE_DIR="$state_dir" \
    "$plugin_root/bin/tmux-agent" plugin rollback 0.1.0 >/dev/null
[[ $("$data_dir/current" --version) == 'tmux-agent 0.1.0' ]]
versions=$(TMUX_AGENT_DATA_DIR="$data_dir" \
    "$plugin_root/bin/tmux-agent" plugin versions)
[[ $versions == *'0.1.0'* && $versions == *'0.1.1'* ]]

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

printf '%s\n' 'installer integration tests passed'
