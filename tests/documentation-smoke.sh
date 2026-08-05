#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=$("$root/scripts/check-version")
for required in \
    README.md \
    THIRD_PARTY_NOTICES.md \
    THIRD_PARTY_LICENSES.html \
    COMPATIBILITY \
    docs/installation.md \
    docs/remote-machines.md \
    docs/troubleshooting.md \
    docs/architecture.md \
    scripts/install \
    scripts/uninstall \
    scripts/doctor \
    scripts/check-version \
    scripts/check-public-tree \
    scripts/generate-third-party-licenses \
    scripts/check-third-party-licenses \
    scripts/check-release-readiness \
    scripts/package-release \
    bin/tmux-agent \
    tmux-agent.tmux; do
    [[ -e $root/$required ]] || {
        printf 'documented release file is missing: %s\n' "$required" >&2
        exit 1
    }
done

cargo build --locked
binary="$root/target/debug/tmux-agent"
help=$("$binary" --help)
for command in daemon list watch ui focus explain acknowledge scan run codex claude opencode pi paths doctor update; do
    [[ $help == *"$command"* ]] || {
        printf 'root help does not expose documented command: %s\n' "$command" >&2
        exit 1
    }
done
daemon_help=$("$binary" daemon --help)
for command in run start status stop restart; do
    [[ $daemon_help == *"$command"* ]] || {
        printf 'daemon help does not expose documented command: %s\n' "$command" >&2
        exit 1
    }
done

test_root=$(mktemp -d "/tmp/tmux-agent-doc-test.XXXXXX")
cleanup() {
    exit_code=$?
    if [[ -n ${watch_pid:-} ]]; then
        kill "$watch_pid" 2>/dev/null || true
        wait "$watch_pid" 2>/dev/null || true
    fi
    if [[ -f $test_root/config/no-server.toml ]]; then
        env \
            HOME="$test_root/home" \
            XDG_RUNTIME_DIR="$test_root/runtime" \
            XDG_STATE_HOME="$test_root/state" \
            XDG_CONFIG_HOME="$test_root/config" \
            "$binary" --config "$test_root/config/no-server.toml" daemon stop \
            >/dev/null 2>&1 || true
    fi
    if ((exit_code != 0)); then
        for log in "$test_root"/state/tmux-agent/*.log; do
            if [[ -f $log ]]; then
                printf 'documentation smoke daemon log: %s\n' "$log" >&2
                sed -n '1,240p' "$log" >&2 || true
            fi
        done
    fi
    rm -rf -- "$test_root"
    return "$exit_code"
}
trap cleanup EXIT
mkdir -p "$test_root/runtime" "$test_root/state" "$test_root/config" \
    "$test_root/home"

TMUX_AGENT_ROOT="$root" \
    XDG_RUNTIME_DIR="$test_root/runtime" \
    XDG_STATE_HOME="$test_root/state" \
    XDG_CONFIG_HOME="$test_root/config" \
    "$binary" --config "$test_root/config/missing.toml" doctor --json \
    >"$test_root/doctor.json"
grep -F "\"application_version\": \"$version\"" "$test_root/doctor.json" >/dev/null
grep -F '"protocol": 3' "$test_root/doctor.json" >/dev/null

cat >"$test_root/config/no-server.toml" <<EOF
tmux_args = ["-L", "tmux-agent-no-server-$$"]
EOF
run_isolated() {
    env \
        HOME="$test_root/home" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        XDG_CONFIG_HOME="$test_root/config" \
        "$binary" --config "$test_root/config/no-server.toml" "$@"
}

run_isolated scan --json \
    >"$test_root/no-server-scan.json"
grep -F '"protocol": 3' "$test_root/no-server-scan.json" >/dev/null
run_isolated paths >"$test_root/paths.txt"
grep -F 'socket ' "$test_root/paths.txt" >/dev/null
run_isolated daemon start >"$test_root/start.txt"
grep -F "running: version $version, protocol 3" "$test_root/start.txt" >/dev/null
run_isolated daemon status >"$test_root/status.txt"
grep -F "running: version $version, protocol 3" "$test_root/status.txt" >/dev/null
run_isolated daemon restart >"$test_root/restart.txt"
grep -F "restarted: version $version" "$test_root/restart.txt" >/dev/null
run_isolated list --json --local-only >"$test_root/list.json"
grep -F "\"application_version\": \"$version\"" "$test_root/list.json" >/dev/null

run_isolated watch --jsonl --local-only >"$test_root/watch.jsonl" &
watch_pid=$!
for _ in {1..50}; do
    [[ -s $test_root/watch.jsonl ]] && break
    sleep 0.1
done
[[ -s $test_root/watch.jsonl ]]
kill "$watch_pid" 2>/dev/null || true
wait "$watch_pid" 2>/dev/null || true
watch_pid=
grep -F "\"application_version\":\"$version\"" "$test_root/watch.jsonl" >/dev/null

for command in focus explain acknowledge; do
    if run_isolated "$command" missing-record \
        >"$test_root/$command.out" 2>"$test_root/$command.err"; then
        printf 'missing records should fail %s\n' "$command" >&2
        exit 1
    fi
    grep -F 'no agent matches' "$test_root/$command.err" >/dev/null
done

run_isolated daemon stop >"$test_root/stop.txt"
grep -F 'stopped:' "$test_root/stop.txt" >/dev/null
run_isolated daemon status >"$test_root/stopped-status.txt"
grep -F 'stopped:' "$test_root/stopped-status.txt" >/dev/null

TMUX_AGENT_DATA_DIR="$test_root/uninstalled/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/uninstalled-state/tmux-agent" \
    "$root/bin/tmux-agent" plugin versions \
    >"$test_root/versions.txt"
grep -F 'no managed versions installed' "$test_root/versions.txt" >/dev/null

if TMUX_AGENT_DATA_DIR="$test_root/uninstalled/tmux-agent" \
    TMUX_AGENT_STATE_DIR="$test_root/uninstalled-state/tmux-agent" \
    "$root/scripts/doctor" --json >"$test_root/preinstall.json"; then
    printf '%s\n' 'preinstall doctor should report a missing binary' >&2
    exit 1
fi
grep -F '"application_version": null' "$test_root/preinstall.json" >/dev/null
grep -F '"install_status": "MISSING"' "$test_root/preinstall.json" >/dev/null

"$root/scripts/check-version" >/dev/null
"$root/scripts/check-public-tree" >/dev/null

mkdir -p "$test_root/public-tree"
printf '%s\n' 'synthetic private marker' >"$test_root/public-tree/fixture.txt"
printf '%s\t%s\n' 'synthetic marker found' 'private marker' \
    >"$test_root/public-tree-denylist"
if TMUX_AGENT_PUBLIC_TREE_DENYLIST="$test_root/public-tree-denylist" \
    "$root/scripts/check-public-tree" "$test_root/public-tree" \
    >"$test_root/public-tree.out" 2>"$test_root/public-tree.err"; then
    printf '%s\n' 'private public-tree denylist should reject matching input' >&2
    exit 1
fi
grep -F 'public-tree check failed: synthetic marker found' \
    "$test_root/public-tree.err" >/dev/null

printf '%s\t%s\n' 'malformed synthetic pattern' '(' \
    >"$test_root/public-tree-denylist"
if TMUX_AGENT_PUBLIC_TREE_DENYLIST="$test_root/public-tree-denylist" \
    "$root/scripts/check-public-tree" "$test_root/public-tree" \
    >"$test_root/public-tree.out" 2>"$test_root/public-tree.err"; then
    printf '%s\n' 'malformed private denylist patterns should fail closed' >&2
    exit 1
fi
grep -F 'public-tree scan error: malformed synthetic pattern' \
    "$test_root/public-tree.err" >/dev/null

printf '%s\n' 'documentation smoke tests passed'
