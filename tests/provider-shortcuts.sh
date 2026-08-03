#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --locked

test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent-provider-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT
test_home="$test_root/user-home"
mkdir -p "$test_root/bin" "$test_home" "$test_root/runtime" "$test_root/state"
printf '%s\n' sentinel >"$test_home/.zshrc"
printf '%s\n' sentinel >"$test_home/.bashrc"

for provider in codex claude opencode; do
    provider_path="$test_root/bin/$provider"
    cat >"$provider_path" <<'EOF'
#!/bin/sh
{
    printf '%s' "$TMUX_AGENT_TEST_PROVIDER"
    for argument in "$@"; do
        printf '\t%s' "$argument"
    done
    printf '\n'
} >>"$TMUX_AGENT_TEST_LOG"
exit "$TMUX_AGENT_TEST_EXIT"
EOF
    chmod +x "$provider_path"
done

run_shortcut() {
    local provider=$1
    local expected_exit=$2
    shift 2
    set +e
    env \
        HOME="$test_home" \
        PATH="$test_root/bin:$PATH" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        TMUX_AGENT_TEST_PROVIDER="$provider" \
        TMUX_AGENT_TEST_LOG="$test_root/providers.log" \
        TMUX_AGENT_TEST_EXIT="$expected_exit" \
        "$root/target/debug/tmux-agent" "$provider" "$@" \
        </dev/null >"$test_root/$provider.out" 2>"$test_root/$provider.err"
    local actual_exit=$?
    set -e
    [[ $actual_exit -eq $expected_exit ]]
}

run_shortcut codex 23 resume session-id --model gpt-test
run_shortcut claude 17 --continue --model sonnet
run_shortcut opencode 0 --help

grep -Fx $'codex\tresume\tsession-id\t--model\tgpt-test' \
    "$test_root/providers.log" >/dev/null
grep -Fx $'claude\t--continue\t--model\tsonnet' \
    "$test_root/providers.log" >/dev/null
grep -Fx $'opencode\t--help' "$test_root/providers.log" >/dev/null
[[ $(<"$test_home/.zshrc") == sentinel ]]
[[ $(<"$test_home/.bashrc") == sentinel ]]
[[ $(find "$test_home" -type f | wc -l | tr -d ' ') == 2 ]]

printf '%s\n' 'provider shortcut tests passed'
