#!/usr/bin/env bash
set -euo pipefail

if (($# != 3)); then
    printf 'usage: %s <target> <candidate-release-root> <v0.3.0-release-root>\n' \
        "$0" >&2
    exit 2
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TMUX_AGENT_ROOT=$root
export TMUX_AGENT_ROOT
# shellcheck source=scripts/lib.sh
. "$root/scripts/lib.sh"
target=$1
candidate_release_root=$(CDPATH='' cd -- "$2" && pwd)
baseline_release_root=$(CDPATH='' cd -- "$3" && pwd)
candidate_version=$("$root/scripts/check-version")
baseline_version=0.3.0

case "$target" in
    aarch64-apple-darwin)
        baseline_sha256=7582170026c4e3eb79b29d0a3bf0ad69fdbd04837d9eb09ae63b214e3ab025d4
        ;;
    x86_64-apple-darwin)
        baseline_sha256=f8b27024e40dafe2d20d6e1e5e490db2a1d3f26af05ae8cf94928d2a120e56e3
        ;;
    aarch64-unknown-linux-gnu)
        baseline_sha256=b8e4b6043dcbd5c0de1feb338e5bc1f6704ba58b056b3e9e93e7b21464c0738b
        ;;
    x86_64-unknown-linux-gnu)
        baseline_sha256=893bec0bea69ae8f7e67e55df51c377d7141f2b1f70f61b69e33a41dcf327336
        ;;
    *)
        printf 'unsupported release lifecycle target: %s\n' "$target" >&2
        exit 2
        ;;
esac

if ! tmux_agent_version_at_least "$candidate_version" "$baseline_version" ||
    tmux_agent_version_at_least "$baseline_version" "$candidate_version"; then
    printf '%s\n' \
        'release lifecycle smoke requires a candidate newer than the v0.3.0 baseline' >&2
    exit 1
fi

baseline_asset="tmux-agent-v${baseline_version}-${target}.tar.gz"
candidate_asset="tmux-agent-v${candidate_version}-${target}.tar.gz"
baseline_dir="$baseline_release_root/v$baseline_version"
candidate_dir="$candidate_release_root/v$candidate_version"
baseline_archive="$baseline_dir/$baseline_asset"
candidate_archive="$candidate_dir/$candidate_asset"

for required in \
    "$baseline_archive" \
    "$baseline_dir/SHA256SUMS" \
    "$candidate_archive" \
    "$candidate_dir/SHA256SUMS"; do
    [[ -s $required ]] || {
        printf 'release lifecycle input is missing or empty: %s\n' "$required" >&2
        exit 1
    }
done

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

recorded_checksum() {
    local sums=$1
    local asset=$2
    awk -v asset="$asset" '
        $2 == asset || $2 == "*" asset {
            checksum = tolower($1)
            matches++
        }
        END {
            if (matches != 1) exit 1
            print checksum
        }
    ' "$sums"
}

verify_archive() {
    local archive=$1
    local sums=$2
    local asset=$3
    local pinned=${4:-}
    local recorded
    local actual
    recorded=$(recorded_checksum "$sums" "$asset") || {
        printf 'SHA256SUMS has no unique entry for %s\n' "$asset" >&2
        exit 1
    }
    [[ $recorded =~ ^[0-9a-f]{64}$ ]] || {
        printf 'SHA256SUMS has an invalid entry for %s\n' "$asset" >&2
        exit 1
    }
    if [[ -n $pinned && $recorded != "$pinned" ]]; then
        printf 'published v0.3.0 checksum changed for %s\n' "$asset" >&2
        exit 1
    fi
    actual=$(checksum "$archive")
    [[ $actual == "$recorded" ]] || {
        printf 'release lifecycle archive checksum mismatch: %s\n' "$asset" >&2
        exit 1
    }
}

verify_archive \
    "$baseline_archive" "$baseline_dir/SHA256SUMS" \
    "$baseline_asset" "$baseline_sha256"
verify_archive \
    "$candidate_archive" "$candidate_dir/SHA256SUMS" "$candidate_asset"

baseline_entries=$(tar -tzf "$baseline_archive")
for required in tmux-agent README.md LICENSE THIRD_PARTY_NOTICES.md \
    THIRD_PARTY_LICENSES.html; do
    grep -qxE "(\\./)?${required}" <<<"$baseline_entries" || {
        printf 'v0.3.0 archive is missing historical entry: %s\n' "$required" >&2
        exit 1
    }
done
if grep -qE '(^|/)(COMPATIBILITY|TARGET)$' <<<"$baseline_entries"; then
    printf '%s\n' 'v0.3.0 archive unexpectedly contains managed metadata' >&2
    exit 1
fi

test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent-release-lifecycle.XXXXXX")
cleanup() {
    local exit_code=$?
    if ((exit_code != 0)); then
        for lifecycle_log in "$test_root"/*/state/tmux-agent/*.log; do
            if [[ -f $lifecycle_log ]]; then
                printf 'release lifecycle daemon log: %s\n' "$lifecycle_log" >&2
                sed -n '1,160p' "$lifecycle_log" >&2 || true
            fi
        done
    fi
    find "$test_root" -depth -delete
    return "$exit_code"
}
trap cleanup EXIT

native_bin="$test_root/native-bin"
mkdir "$native_bin"
for native_tool in \
    awk bash cat chmod cmp cp curl cut date dirname env grep head hostname kill ln lsof \
    mkdir mktemp mv pgrep ps readlink rm rmdir sed sh shasum sha256sum sleep \
    sort stat tail tar tmux tr uname wc wget xargs; do
    native_tool_path=$(command -v "$native_tool" 2>/dev/null || true)
    case "$native_tool_path" in
        /*) ln -s "$native_tool_path" "$native_bin/$native_tool" ;;
    esac
done
for required_native_tool in \
    awk bash chmod cp curl dirname env grep hostname ln mkdir mktemp mv ps \
    readlink rm rmdir sh sleep tar tmux uname; do
    [[ -x $native_bin/$required_native_tool ]] || {
        printf 'release lifecycle runtime tool is unavailable: %s\n' \
            "$required_native_tool" >&2
        exit 1
    }
done
if [[ -e $native_bin/cargo || -e $native_bin/rustc ]]; then
    printf '%s\n' 'release lifecycle runtime PATH unexpectedly contains Rust' >&2
    exit 1
fi

candidate_release_url="file://${candidate_release_root// /%20}"

prepare_scenario() {
    local name=$1
    scenario_root="$test_root/$name"
    export HOME="$scenario_root/home"
    export XDG_CONFIG_HOME="$scenario_root/config"
    export XDG_DATA_HOME="$scenario_root/data"
    export XDG_RUNTIME_DIR="$scenario_root/runtime"
    export XDG_STATE_HOME="$scenario_root/state"
    export TMUX_AGENT_DATA_DIR="$XDG_DATA_HOME/tmux-agent"
    export TMUX_AGENT_STATE_DIR="$XDG_STATE_HOME/tmux-agent"
    export TMUX_AGENT_RELEASE_BASE_URL="$candidate_release_url"
    config_file="$XDG_CONFIG_HOME/tmux-agent/config.toml"
    launcher="$HOME/.local/bin/tmux-agent"
    tmux_socket="tmux-agent-lifecycle-${name}-$$"
    cleanup_agent=
    background_pid=
    held_lock_dir=
    lock_owner_pid=
    export PATH="$native_bin"
    if command -v cargo >/dev/null 2>&1 || command -v rustc >/dev/null 2>&1; then
        printf '%s\n' 'release lifecycle scenario found Rust on its runtime PATH' >&2
        exit 1
    fi
    mkdir -p "$HOME" "$XDG_CONFIG_HOME/tmux-agent" "$XDG_RUNTIME_DIR" \
        "$TMUX_AGENT_STATE_DIR"
    chmod 700 "$HOME" "$XDG_CONFIG_HOME/tmux-agent" "$XDG_RUNTIME_DIR" \
        "$TMUX_AGENT_STATE_DIR"
    cat >"$config_file" <<EOF
tmux_args = ["-L", "$tmux_socket"]
EOF
}

cleanup_scenario() {
    if [[ -n ${background_pid:-} ]]; then
        if kill -0 "$background_pid" 2>/dev/null; then
            kill "$background_pid" 2>/dev/null || true
        fi
        wait "$background_pid" 2>/dev/null || true
    fi
    if [[ -n ${lock_owner_pid:-} ]]; then
        if kill -0 "$lock_owner_pid" 2>/dev/null; then
            kill "$lock_owner_pid" 2>/dev/null || true
        fi
        wait "$lock_owner_pid" 2>/dev/null || true
    fi
    if [[ -n ${held_lock_dir:-} && $held_lock_dir == "$TMUX_AGENT_DATA_DIR/.install.lock" ]]; then
        rm -f -- "$held_lock_dir/pid"
        rmdir "$held_lock_dir" 2>/dev/null || true
    fi
    if [[ -n ${tmux_socket:-} ]]; then
        tmux -L "$tmux_socket" kill-server >/dev/null 2>&1 || true
    fi
    if [[ -n ${cleanup_agent:-} && -x $cleanup_agent ]]; then
        local daemon_pids=
        local daemon_pid
        for _ in {1..100}; do
            "$cleanup_agent" --config "$config_file" daemon stop \
                >/dev/null 2>&1 || true
            daemon_pids=$(ps -ww -axo pid=,command= |
                awk -v config="$config_file" \
                    'index($0, config) && / daemon run$/ { print $1 }')
            [[ -z $daemon_pids ]] && break
            sleep 0.1
        done
        for daemon_pid in $daemon_pids; do
            kill "$daemon_pid" 2>/dev/null || true
        done
    fi
}

release_held_lock() {
    [[ -n ${held_lock_dir:-} && $held_lock_dir == "$TMUX_AGENT_DATA_DIR/.install.lock" ]]
    rm -f -- "$held_lock_dir/pid"
    rmdir "$held_lock_dir"
    held_lock_dir=
    if [[ -n ${lock_owner_pid:-} ]]; then
        kill "$lock_owner_pid" 2>/dev/null || true
        wait "$lock_owner_pid" 2>/dev/null || true
        lock_owner_pid=
    fi
}

prepare_network_sentinels() {
    network_bin="$scenario_root/no-network-bin"
    network_log="$scenario_root/network-client.log"
    mkdir "$network_bin"
    for network_client in curl wget; do
        cat >"$network_bin/$network_client" <<'EOF'
#!/bin/sh
printf '%s\n' "$0 $*" >>"${TMUX_AGENT_NETWORK_TEST_LOG:?}"
exit 97
EOF
        chmod 755 "$network_bin/$network_client"
    done
}

assert_update_without_download() {
    local runner=$1
    local requested_version=$2
    local expected_message=$3
    local update_output
    rm -f -- "$network_log"
    update_output=$(
        PATH="$network_bin:$PATH" \
            TMUX_AGENT_NETWORK_TEST_LOG="$network_log" \
            "$runner" update --version "$requested_version"
    )
    grep -F "$expected_message" <<<"$update_output" >/dev/null
    [[ ! -e $network_log ]]
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
}

assert_live_lock_serialization() {
    local runner=$1
    local update_output="$scenario_root/locked-update.out"
    local update_error="$scenario_root/locked-update.err"
    held_lock_dir="$TMUX_AGENT_DATA_DIR/.install.lock"
    mkdir "$held_lock_dir"
    sleep 30 &
    lock_owner_pid=$!
    printf '%s\n' "$lock_owner_pid" >"$held_lock_dir/pid"
    chmod 600 "$held_lock_dir/pid"

    rm -f -- "$network_log"
    PATH="$network_bin:$PATH" \
        TMUX_AGENT_NETWORK_TEST_LOG="$network_log" \
        "$runner" update --version "$candidate_version" \
        >"$update_output" 2>"$update_error" &
    background_pid=$!
    sleep 0.3
    if ! kill -0 "$background_pid" 2>/dev/null; then
        wait "$background_pid" 2>/dev/null || true
        printf '%s\n' 'launcher-routed update did not wait for the live installation lock' >&2
        sed -n '1,80p' "$update_error" >&2
        exit 1
    fi
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    [[ ! -e $network_log ]]

    release_held_lock
    if ! wait "$background_pid"; then
        background_pid=
        printf '%s\n' 'launcher-routed update failed after the lock was released' >&2
        sed -n '1,80p' "$update_error" >&2
        exit 1
    fi
    background_pid=
    grep -F "version $candidate_version is already current" \
        "$update_output" >/dev/null
    [[ ! -e $network_log ]]
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
}

assert_selection() {
    local name=$1
    local version=$2
    [[ $(readlink "$TMUX_AGENT_DATA_DIR/$name") == "versions/$version/tmux-agent" ]]
}

assert_version_metadata() {
    local version=$1
    local management=$2
    local version_dir="$TMUX_AGENT_DATA_DIR/versions/$version"
    [[ $("$version_dir/tmux-agent" --version) == "tmux-agent $version" ]]
    [[ $(<"$version_dir/TARGET") == "$target" ]]
    grep -Fx "launcher_protocol=1" "$version_dir/COMPATIBILITY" >/dev/null
    grep -Fx "binary_version=$version" "$version_dir/COMPATIBILITY" >/dev/null
    if [[ $management == yes ]]; then
        grep -Fx 'management_protocol=1' "$version_dir/COMPATIBILITY" >/dev/null
    elif grep -q '^management_protocol=' "$version_dir/COMPATIBILITY"; then
        printf 'legacy runtime gained lifecycle-controller capability: %s\n' \
            "$version" >&2
        exit 1
    fi
}

wait_for_manager() {
    for _ in {1..300}; do
        if [[ -L $TMUX_AGENT_DATA_DIR/manager ]] &&
            [[ $("$TMUX_AGENT_DATA_DIR/manager" --version 2>/dev/null || true) == "tmux-agent $candidate_version" ]]; then
            return 0
        fi
        sleep 0.1
    done
    printf 'timed out waiting for TPM lifecycle controller: %s\n' \
        "$TMUX_AGENT_DATA_DIR" >&2
    return 1
}

run_tpm_checkout() {
    tmux -L "$tmux_socket" -f /dev/null new-session -d -s lifecycle
    tmux -L "$tmux_socket" set-option -g @tmux-agent-auto-start off
    tmux -L "$tmux_socket" run-shell "$root/tmux-agent.tmux"
    wait_for_manager
    tmux -L "$tmux_socket" list-keys -T prefix |
        awk '$4 == "A" && index($0, "scripts/launch-popup") { found = 1 }
             END { exit !found }'
}

(
    prepare_scenario standalone-fresh
    trap cleanup_scenario EXIT
    TMUX_AGENT_INSTALL_PATH="$launcher" "$root/scripts/install" --no-restart \
        >/dev/null
    cleanup_agent=$launcher
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    grep -Fx "active    $candidate_version" < <("$launcher" versions) >/dev/null
    prepare_network_sentinels
    assert_update_without_download \
        "$launcher" "$candidate_version" \
        "version $candidate_version is already current"
    assert_update_without_download \
        "$launcher" "$baseline_version" \
        "newer version $candidate_version is already current; not replacing it with $baseline_version"
    assert_live_lock_serialization "$launcher"
    "$launcher" --config "$config_file" daemon start >/dev/null
    "$launcher" --config "$config_file" daemon status |
        grep -F "running: version $candidate_version" >/dev/null
    "$launcher" --config "$config_file" doctor --json |
        grep -F "\"application_version\": \"$candidate_version\"" >/dev/null
)

(
    prepare_scenario standalone-upgrade
    trap cleanup_scenario EXIT
    mkdir -p "${launcher%/*}"
    tar -xOf "$baseline_archive" tmux-agent >"$launcher"
    chmod 755 "$launcher"
    [[ $($launcher --version) == "tmux-agent $baseline_version" ]]
    TMUX_AGENT_INSTALL_PATH="$launcher" "$root/scripts/install" --no-restart \
        >/dev/null
    cleanup_agent=$launcher
    assert_selection current "$baseline_version"
    assert_selection manager "$candidate_version"
    assert_version_metadata "$baseline_version" no
    assert_version_metadata "$candidate_version" yes
    versions=$("$launcher" versions)
    [[ $versions == *"active    $baseline_version"* ]]
    [[ $versions == *"rollback  $candidate_version"* ]]
    "$launcher" --config "$config_file" rollback "$candidate_version" >/dev/null
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    "$launcher" --config "$config_file" daemon status |
        grep -F "running: version $candidate_version" >/dev/null
    "$launcher" versions | grep -F "active    $candidate_version" >/dev/null

    invalid_config="$scenario_root/invalid-config.toml"
    printf '%s\n' 'tmux_args = [' >"$invalid_config"
    if "$launcher" --config "$invalid_config" rollback "$baseline_version" \
        >"$scenario_root/failed-restart.out" \
        2>"$scenario_root/failed-restart.err"; then
        printf '%s\n' 'rollback with an invalid restart config unexpectedly succeeded' >&2
        exit 1
    fi
    grep -F 'previous activation was restored' \
        "$scenario_root/failed-restart.err" >/dev/null
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    "$launcher" --config "$config_file" daemon status |
        grep -F "running: version $candidate_version" >/dev/null

    "$launcher" --config "$config_file" rollback "$baseline_version" >/dev/null
    assert_selection current "$baseline_version"
    assert_selection manager "$candidate_version"
    "$launcher" --config "$config_file" daemon status |
        grep -F "running: version $baseline_version" >/dev/null
    "$launcher" --config "$config_file" rollback "$candidate_version" >/dev/null
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    "$launcher" --config "$config_file" daemon status |
        grep -F "running: version $candidate_version" >/dev/null
)

(
    prepare_scenario tpm-fresh
    trap cleanup_scenario EXIT
    run_tpm_checkout
    cleanup_agent="$root/bin/tmux-agent"
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    grep -Fx "active    $candidate_version" < \
        <("$root/bin/tmux-agent" versions) >/dev/null
)

(
    prepare_scenario tpm-v0.3-upgrade
    trap cleanup_scenario EXIT
    legacy_dir="$TMUX_AGENT_DATA_DIR/versions/$baseline_version"
    mkdir -p "$legacy_dir"
    tar -xzf "$baseline_archive" -C "$legacy_dir"
    chmod 700 "$TMUX_AGENT_DATA_DIR" "$TMUX_AGENT_DATA_DIR/versions" \
        "$legacy_dir"
    ln -s "versions/$baseline_version/tmux-agent" \
        "$TMUX_AGENT_DATA_DIR/current"

    run_tpm_checkout
    cleanup_agent="$root/bin/tmux-agent"
    assert_selection current "$baseline_version"
    assert_selection manager "$candidate_version"
    assert_version_metadata "$baseline_version" no
    assert_version_metadata "$candidate_version" yes
    versions=$("$root/bin/tmux-agent" versions)
    [[ $versions == *"active    $baseline_version"* ]]
    [[ $versions == *"rollback  $candidate_version"* ]]

    "$root/bin/tmux-agent" --config "$config_file" \
        rollback "$candidate_version" >/dev/null
    assert_selection current "$candidate_version"
    assert_selection manager "$candidate_version"
    "$root/bin/tmux-agent" --config "$config_file" daemon status |
        grep -F "running: version $candidate_version" >/dev/null

    "$root/bin/tmux-agent" --config "$config_file" \
        rollback "$baseline_version" >/dev/null
    assert_selection current "$baseline_version"
    assert_selection manager "$candidate_version"
    "$root/bin/tmux-agent" versions |
        grep -F "active    $baseline_version" >/dev/null
    "$root/bin/tmux-agent" --config "$config_file" daemon status |
        grep -F "running: version $baseline_version" >/dev/null
)

printf 'release lifecycle smoke passed for %s: v%s -> v%s\n' \
    "$target" "$baseline_version" "$candidate_version"
