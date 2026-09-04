#!/usr/bin/env bash
set -euo pipefail

scenario=${1:-}
case "$scenario" in
    standalone | plugin)
        ;;
    *)
        printf 'usage: %s {standalone|plugin}\n' "$0" >&2
        exit 2
        ;;
esac

root=/opt/tmux-agent
version=$(sed -n '1p' "$root/VERSION")
socket_name="tmux-agent-fresh-${scenario}-$$"
session_name=fresh-install
http_port=$((18000 + ($$ % 1000)))
release_url="http://127.0.0.1:${http_port}"

export HOME="/home/user"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_RUNTIME_DIR="$HOME/.local/run"
export XDG_STATE_HOME="$HOME/.local/state"
export TMUX_AGENT_RELEASE_BASE_URL="$release_url"

data_dir="$XDG_DATA_HOME/tmux-agent"
state_dir="$XDG_STATE_HOME/tmux-agent"
config_dir="$XDG_CONFIG_HOME/tmux-agent"
agent="$root/bin/tmux-agent"
http_pid=
client_pid=

cleanup() {
    exit_code=$?
    if [[ -x $data_dir/current ]]; then
        "$data_dir/current" daemon stop >/dev/null 2>&1 || true
    fi
    tmux -L "$socket_name" kill-server >/dev/null 2>&1 || true
    if [[ -n $http_pid ]]; then
        kill "$http_pid" >/dev/null 2>&1 || true
        wait "$http_pid" 2>/dev/null || true
    fi
    if [[ -n $client_pid ]]; then
        kill "$client_pid" >/dev/null 2>&1 || true
        wait "$client_pid" 2>/dev/null || true
    fi
    if ((exit_code != 0)); then
        printf 'fresh-install scenario failed: %s\n' "$scenario" >&2
        if [[ -f /tmp/tmux-agent-http.log ]]; then
            sed -n '1,120p' /tmp/tmux-agent-http.log >&2 || true
        fi
        for log_file in "$state_dir"/*.log; do
            if [[ -f $log_file ]]; then
                printf 'tmux-agent log: %s\n' "$log_file" >&2
                sed -n '1,200p' "$log_file" >&2 || true
            fi
        done
    fi
    return "$exit_code"
}
trap cleanup EXIT

wait_for() {
    local description=$1
    shift
    for _ in {1..600}; do
        if "$@"; then
            return 0
        fi
        sleep 0.1
    done
    printf 'timed out waiting for %s\n' "$description" >&2
    return 1
}

current_ready() {
    [[ -x $data_dir/current ]] &&
        [[ $($data_dir/current --version 2>/dev/null) == "tmux-agent $version" ]]
}

daemon_ready() {
    "$agent" daemon status 2>/dev/null |
        grep -F "running: version $version, protocol 4" >/dev/null
}

client_ready() {
    tmux -L "$socket_name" list-clients >/dev/null 2>&1
}

popup_process_ready() {
    pgrep -f "$data_dir/current ui --popup" >/dev/null 2>&1
}

client_stopped() {
    ! kill -0 "$client_pid" 2>/dev/null
}

case "${TMUX_AGENT_EXPECTED_ARCH:-}" in
    arm64)
        [[ $(uname -m) == aarch64 ]]
        expected_target=aarch64-unknown-linux-gnu
        ;;
    amd64)
        [[ $(uname -m) == x86_64 ]]
        expected_target=x86_64-unknown-linux-gnu
        ;;
    *)
        printf 'unsupported Docker architecture: %s\n' \
            "${TMUX_AGENT_EXPECTED_ARCH:-missing}" >&2
        exit 1
        ;;
esac

[[ ! -e $data_dir ]]
[[ ! -e $state_dir ]]
[[ ! -e $config_dir ]]
mkdir -p "$XDG_RUNTIME_DIR" "$config_dir"
chmod 0700 "$XDG_RUNTIME_DIR" "$config_dir"
cat >"$config_dir/config.toml" <<EOF
tmux_args = ["-L", "$socket_name"]
EOF
printf '%s\n' preserve-me >"$config_dir/user-setting"

python3 -m http.server "$http_port" --bind 127.0.0.1 \
    --directory /srv/releases >/tmp/tmux-agent-http.log 2>&1 &
http_pid=$!
wait_for 'release HTTP server' \
    curl --fail --silent --output /dev/null \
    "$release_url/v${version}/SHA256SUMS"

tmux -L "$socket_name" -f /dev/null new-session -d \
    -s "$session_name" -n shell 'sleep 300'

verify_runtime() {
    local doctor_json=$HOME/doctor.json
    local list_json=$HOME/list.json
    local ui_capture=$HOME/ui.txt

    [[ $($agent --version) == "tmux-agent $version" ]]
    [[ $(readlink "$data_dir/current") == "versions/$version/tmux-agent" ]]
    [[ $(readlink "$data_dir/manager") == "versions/$version/tmux-agent" ]]
    [[ $(stat -c '%a' "$data_dir") == 700 ]]
    [[ $(stat -c '%a' "$data_dir/.tmux-agent-managed") == 600 ]]
    for installed_file in README.md LICENSE THIRD_PARTY_NOTICES.md \
        THIRD_PARTY_LICENSES.html; do
        [[ -s $data_dir/versions/$version/$installed_file ]]
    done

    "$agent" daemon start >/dev/null
    wait_for 'tmux-agent daemon' daemon_ready
    "$agent" doctor --json >"$doctor_json"
    grep -F "\"application_version\": \"$version\"" "$doctor_json" >/dev/null
    grep -F '"protocol": 4' "$doctor_json" >/dev/null
    grep -F "$expected_target" "$doctor_json" >/dev/null
    "$agent" list --json --local-only >"$list_json"
    grep -F "\"application_version\": \"$version\"" "$list_json" >/dev/null

    tmux -L "$socket_name" new-window -d -t "$session_name:" \
        -n tmux-agent-ui "exec '$agent' ui"
    wait_for 'tmux-agent UI rendering' bash -c \
        "tmux -L '$socket_name' capture-pane -p -t '$session_name:tmux-agent-ui.0' | grep -F 'tmux-agent' >/dev/null"
    tmux -L "$socket_name" capture-pane -p \
        -t "$session_name:tmux-agent-ui.0" >"$ui_capture"
    grep -F 'tmux-agent' "$ui_capture" >/dev/null
    tmux -L "$socket_name" send-keys \
        -t "$session_name:tmux-agent-ui.0" q
}

case "$scenario" in
    standalone)
        "$root/scripts/install" --no-restart >"$HOME/install.out"
        grep -F "tmux-agent: ready at $data_dir/current" \
            "$HOME/install.out" >/dev/null
        installed_agent="$HOME/.local/bin/tmux-agent"
        [[ -x $installed_agent ]]
        grep -Fq '# tmux-agent managed launcher' "$installed_agent"
        [[ $($installed_agent --version) == "tmux-agent $version" ]]
        grep -Fx "active    $version" < <("$installed_agent" versions)
        verify_runtime

        "$root/scripts/install" --no-restart >"$HOME/reinstall.out"
        grep -F "tmux-agent: ready at $data_dir/current" \
            "$HOME/reinstall.out" >/dev/null
        ;;
    plugin)
        tmux -L "$socket_name" kill-server
        cat >"$HOME/.tmux.conf" <<EOF
set -g @plugin 'hypertectonic/tmux-agent'
set -g @tmux-agent-key 'A'
set -g @tmux-agent-popup-width '70%'
set -g @tmux-agent-popup-height '70%'
bind-key B display-message 'preserved-binding'
run-shell '$root/tmux-agent.tmux'
EOF
        tmux -L "$socket_name" -f "$HOME/.tmux.conf" new-session -d \
            -s "$session_name" -n shell 'sleep 300'

        wait_for 'plugin-managed binary installation' current_ready
        wait_for 'plugin-managed daemon startup' daemon_ready
        verify_runtime

        binding=$(
            tmux -L "$socket_name" list-keys -T prefix |
                awk '$4 == "A" { print; exit }'
        )
        [[ $binding == *launch-popup* ]]
        preserved=$(
            tmux -L "$socket_name" list-keys -T prefix |
                awk '$4 == "B" { print; exit }'
        )
        [[ $preserved == *preserved-binding* ]]

        client_log="$HOME/popup.typescript"
        client_input="$HOME/popup.input"
        mkfifo "$client_input"
        script -q -e -f \
            -c "tmux -L '$socket_name' attach-session -t '$session_name'" \
            "$client_log" <"$client_input" >/dev/null &
        client_pid=$!
        exec 3>"$client_input"
        wait_for 'attached tmux client' client_ready
        wait_for 'tmux client initial draw' \
            grep -aFq 'shell*' "$client_log"
        printf '\002A' >&3
        wait_for 'tmux-agent popup process' popup_process_ready
        wait_for 'tmux-agent popup rendering' \
            grep -aFq 'tmux-agent' "$client_log"
        printf 'q' >&3
        sleep 1
        tmux -L "$socket_name" display-popup -C >/dev/null 2>&1 || true
        printf '\002d' >&3
        exec 3>&-
        wait_for 'tmux client detaching' client_stopped
        wait "$client_pid"
        client_pid=

        tmux -L "$socket_name" source-file "$HOME/.tmux.conf"
        [[ $(tmux -L "$socket_name" list-keys -T prefix |
            awk '$4 == "A" { count++ } END { print count + 0 }') == 1 ]]
        [[ $(tmux -L "$socket_name" list-windows -t "$session_name" |
            wc -l | tr -d ' ') == 1 ]]
        [[ $(tmux -L "$socket_name" list-panes -a |
            wc -l | tr -d ' ') == 1 ]]
        ;;
esac

printf '%s\n' keep-state >"$state_dir/user-state"
"$root/scripts/uninstall" >"$HOME/uninstall.out"
[[ ! -e $data_dir ]]
[[ -f $config_dir/config.toml ]]
[[ $(cat "$config_dir/user-setting") == preserve-me ]]
[[ $(cat "$state_dir/user-state") == keep-state ]]

os_name=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release)
os_name=${os_name#\"}
os_name=${os_name%\"}
printf 'fresh-install %s passed on %s (%s)\n' \
    "$scenario" "$os_name" "$(uname -m)"
