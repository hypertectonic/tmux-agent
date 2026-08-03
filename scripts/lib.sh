#!/bin/sh

tmux_agent_data_dir() {
    if [ -n "${TMUX_AGENT_DATA_DIR:-}" ]; then
        printf '%s\n' "$TMUX_AGENT_DATA_DIR"
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        printf '%s\n' "$XDG_DATA_HOME/tmux-agent"
    else
        printf '%s\n' "${HOME:?HOME is required}/.local/share/tmux-agent"
    fi
}

tmux_agent_state_dir() {
    if [ -n "${TMUX_AGENT_STATE_DIR:-}" ]; then
        printf '%s\n' "$TMUX_AGENT_STATE_DIR"
    elif [ -n "${XDG_STATE_HOME:-}" ]; then
        printf '%s\n' "$XDG_STATE_HOME/tmux-agent"
    else
        printf '%s\n' "${HOME:?HOME is required}/.local/state/tmux-agent"
    fi
}

tmux_agent_version() {
    version_file="${TMUX_AGENT_ROOT:?TMUX_AGENT_ROOT is required}/VERSION"
    [ -r "$version_file" ] || {
        printf 'tmux-agent: missing VERSION at %s\n' "$version_file" >&2
        return 1
    }
    IFS= read -r version <"$version_file"
    case "$version" in
        '' | *[!0-9A-Za-z.+-]*)
            printf 'tmux-agent: invalid VERSION value\n' >&2
            return 1
            ;;
    esac
    printf '%s\n' "$version"
}

tmux_agent_target() {
    system_name=${TMUX_AGENT_UNAME_S:-$(uname -s)}
    machine_name=${TMUX_AGENT_UNAME_M:-$(uname -m)}
    case "$system_name:$machine_name" in
        Darwin:arm64 | Darwin:aarch64)
            printf '%s\n' aarch64-apple-darwin
            ;;
        Darwin:x86_64 | Darwin:amd64)
            printf '%s\n' x86_64-apple-darwin
            ;;
        Linux:x86_64 | Linux:amd64)
            printf '%s\n' x86_64-unknown-linux-gnu
            ;;
        Linux:aarch64 | Linux:arm64)
            printf '%s\n' aarch64-unknown-linux-gnu
            ;;
        *)
            printf 'tmux-agent: unsupported platform %s/%s\n' "$system_name" "$machine_name" >&2
            return 1
            ;;
    esac
}

tmux_agent_binary_matches() {
    binary_path=$1
    expected_version=$2
    [ -x "$binary_path" ] || return 1
    reported_version=$("$binary_path" --version 2>/dev/null) || return 1
    [ "$reported_version" = "tmux-agent $expected_version" ]
}

tmux_agent_release_base_url() {
    if [ -n "${TMUX_AGENT_RELEASE_BASE_URL:-}" ]; then
        printf '%s\n' "${TMUX_AGENT_RELEASE_BASE_URL%/}"
        return 0
    fi
    if [ -n "${TMUX_AGENT_GITHUB_REPOSITORY:-}" ]; then
        printf 'https://github.com/%s/releases/download\n' "${TMUX_AGENT_GITHUB_REPOSITORY%.git}"
        return 0
    fi
    command -v git >/dev/null 2>&1 || {
        printf '%s\n' 'tmux-agent: cannot determine release URL because git is unavailable' >&2
        return 1
    }
    origin=$(git -C "$TMUX_AGENT_ROOT" config --get remote.origin.url 2>/dev/null || true)
    case "$origin" in
        https://github.com/*)
            repository=${origin#https://github.com/}
            repository=${repository%.git}
            ;;
        git@github.com:*)
            repository=${origin#git@github.com:}
            repository=${repository%.git}
            ;;
        ssh://git@github.com/*)
            repository=${origin#ssh://git@github.com/}
            repository=${repository%.git}
            ;;
        *)
            printf '%s\n' \
                'tmux-agent: cannot determine the GitHub release URL; set TMUX_AGENT_RELEASE_BASE_URL' >&2
            return 1
            ;;
    esac
    printf 'https://github.com/%s/releases/download\n' "$repository"
}

tmux_agent_download() {
    source_url=$1
    destination=$2
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error \
            --connect-timeout 10 --max-time 300 \
            --output "$destination" "$source_url"
    elif command -v wget >/dev/null 2>&1; then
        wget --quiet --timeout=300 --output-document="$destination" "$source_url"
    else
        printf '%s\n' 'tmux-agent: install requires curl or wget' >&2
        return 1
    fi
}

tmux_agent_sha256() {
    file_path=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print $1}'
    else
        printf '%s\n' 'tmux-agent: checksum verification requires sha256sum or shasum' >&2
        return 1
    fi
}

tmux_agent_write_status() {
    status_value=$1
    data_dir=$(tmux_agent_data_dir)
    mkdir -p "$data_dir"
    chmod 700 "$data_dir"
    status_tmp="$data_dir/.install-status.$$"
    printf '%s\n' "$status_value" >"$status_tmp"
    chmod 600 "$status_tmp"
    mv -f "$status_tmp" "$data_dir/install-status"
}

tmux_agent_read_status() {
    status_file="$(tmux_agent_data_dir)/install-status"
    if [ -r "$status_file" ]; then
        IFS= read -r status_value <"$status_file"
        printf '%s\n' "$status_value"
    else
        printf '%s\n' MISSING
    fi
}

tmux_agent_current_binary() {
    current_path="$(tmux_agent_data_dir)/current"
    [ -x "$current_path" ] || return 1
    printf '%s\n' "$current_path"
}

tmux_agent_install_log() {
    printf '%s\n' "$(tmux_agent_state_dir)/install.log"
}
