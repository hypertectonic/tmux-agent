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

tmux_agent_is_standalone_launcher() (
    launcher_path=$1
    [ -f "$launcher_path" ] && [ ! -L "$launcher_path" ] &&
        [ -x "$launcher_path" ] || exit 1
    awk '
        NR == 1 && $0 != "#!/bin/sh" { exit 1 }
        NR == 2 && $0 != "# tmux-agent managed launcher" { exit 1 }
        NR == 3 && $0 != "# tmux-agent-standalone-launcher-protocol=1" { exit 1 }
        NR == 3 { found = 1; exit }
        END { if (!found) exit 1 }
    ' "$launcher_path"
)

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

tmux_agent_compatibility_contract() (
    compatibility_file="${TMUX_AGENT_ROOT:?TMUX_AGENT_ROOT is required}/COMPATIBILITY"
    [ -r "$compatibility_file" ] || {
        printf 'tmux-agent: missing compatibility contract at %s\n' \
            "$compatibility_file" >&2
        exit 1
    }
    awk '
        /^[[:space:]]*$/ || /^[[:space:]]*#/ { next }
        /^launcher_protocol=[1-9][0-9]*$/ {
            if (protocol != "") invalid = 1
            protocol = substr($0, index($0, "=") + 1)
            next
        }
        /^minimum_binary_version=[0-9A-Za-z.+-]+$/ {
            if (minimum != "") invalid = 1
            minimum = substr($0, index($0, "=") + 1)
            next
        }
        { invalid = 1 }
        END {
            if (invalid || protocol == "" || minimum == "") exit 1
            print protocol "|" minimum
        }
    ' "$compatibility_file" || {
        printf 'tmux-agent: invalid compatibility contract at %s\n' \
            "$compatibility_file" >&2
        exit 1
    }
)

tmux_agent_version_at_least() (
    candidate=$1
    minimum=$2
    awk -v candidate="$candidate" -v minimum="$minimum" '
        function numeric(value) {
            return value ~ /^[0-9][0-9]*$/
        }
        function compare_numeric(left, right, left_trimmed, right_trimmed) {
            left_trimmed = left
            right_trimmed = right
            sub(/^0+/, "", left_trimmed)
            sub(/^0+/, "", right_trimmed)
            if (left_trimmed == "") left_trimmed = "0"
            if (right_trimmed == "") right_trimmed = "0"
            if (length(left_trimmed) != length(right_trimmed))
                return length(left_trimmed) < length(right_trimmed) ? -1 : 1
            if (left_trimmed == right_trimmed) return 0
            return left_trimmed < right_trimmed ? -1 : 1
        }
        function valid_identifiers(value, reject_numeric_leading_zero,
            identifiers, count, index_value) {
            if (value == "") return 0
            count = split(value, identifiers, ".")
            for (index_value = 1; index_value <= count; index_value++) {
                if (identifiers[index_value] !~ /^[0-9A-Za-z-][0-9A-Za-z-]*$/)
                    return 0
                if (reject_numeric_leading_zero &&
                    identifiers[index_value] ~ /^0[0-9]/)
                    return 0
            }
            return 1
        }
        function parse(version, fields, core, count, index_value, build_index,
            build) {
            build_index = index(version, "+")
            if (build_index > 0) {
                build = substr(version, build_index + 1)
                version = substr(version, 1, build_index - 1)
                if (!valid_identifiers(build, 0)) return 0
            }
            fields["prerelease"] = ""
            index_value = index(version, "-")
            if (index_value > 0) {
                fields["prerelease"] = substr(version, index_value + 1)
                version = substr(version, 1, index_value - 1)
                if (!valid_identifiers(fields["prerelease"], 1)) return 0
            }
            count = split(version, core, ".")
            if (count != 3) return 0
            for (index_value = 1; index_value <= 3; index_value++) {
                if (!numeric(core[index_value])) return 0
                if (core[index_value] ~ /^0[0-9]/) return 0
                fields[index_value] = core[index_value]
            }
            return 1
        }
        function compare_prerelease(left, right, left_parts, right_parts,
            left_count, right_count, count, index_value, result,
            left_numeric, right_numeric) {
            if (left == "" && right == "") return 0
            if (left == "") return 1
            if (right == "") return -1
            left_count = split(left, left_parts, ".")
            right_count = split(right, right_parts, ".")
            count = left_count > right_count ? left_count : right_count
            for (index_value = 1; index_value <= count; index_value++) {
                if (index_value > left_count) return -1
                if (index_value > right_count) return 1
                if (left_parts[index_value] == "" || right_parts[index_value] == "")
                    return 2
                left_numeric = numeric(left_parts[index_value])
                right_numeric = numeric(right_parts[index_value])
                if (left_numeric && right_numeric) {
                    result = compare_numeric(left_parts[index_value], right_parts[index_value])
                } else if (left_numeric) {
                    result = -1
                } else if (right_numeric) {
                    result = 1
                } else if (left_parts[index_value] == right_parts[index_value]) {
                    result = 0
                } else {
                    result = left_parts[index_value] < right_parts[index_value] ? -1 : 1
                }
                if (result != 0) return result
            }
            return 0
        }
        BEGIN {
            if (!parse(candidate, candidate_fields) ||
                !parse(minimum, minimum_fields)) exit 2
            for (component = 1; component <= 3; component++) {
                result = compare_numeric(candidate_fields[component], minimum_fields[component])
                if (result < 0) exit 1
                if (result > 0) exit 0
            }
            result = compare_prerelease(candidate_fields["prerelease"], minimum_fields["prerelease"])
            if (result == 2) exit 2
            exit result < 0 ? 1 : 0
        }
    '
)

tmux_agent_binary_version() (
    binary_path=$1
    [ -x "$binary_path" ] || exit 1
    reported_version=$("$binary_path" --version 2>/dev/null) || exit 1
    case "$reported_version" in
        'tmux-agent '*) binary_version=${reported_version#tmux-agent } ;;
        *) exit 1 ;;
    esac
    tmux_agent_version_at_least "$binary_version" "$binary_version" || exit 1
    printf '%s\n' "$binary_version"
)

tmux_agent_version_compatibility() (
    version_dir=$1
    metadata_file="$version_dir/COMPATIBILITY"
    [ -r "$metadata_file" ] || exit 1
    awk '
        /^launcher_protocol=[1-9][0-9]*$/ {
            if (protocol != "") invalid = 1
            protocol = substr($0, index($0, "=") + 1)
            next
        }
        /^binary_version=[0-9A-Za-z.+-]+$/ {
            if (version != "") invalid = 1
            version = substr($0, index($0, "=") + 1)
            next
        }
        /^management_protocol=[1-9][0-9]*$/ {
            if (management != "") invalid = 1
            management = substr($0, index($0, "=") + 1)
            next
        }
        { invalid = 1 }
        END {
            if (invalid || protocol == "" || version == "") exit 1
            print protocol "|" version
        }
    ' "$metadata_file"
)

tmux_agent_write_version_compatibility() (
    version_dir=$1
    binary_version=$2
    management_protocol=${3:-}
    contract=$(tmux_agent_compatibility_contract) || exit 1
    launcher_protocol=${contract%%|*}
    metadata_tmp="$version_dir/.compatibility.$$"
    trap 'rm -f -- "$metadata_tmp"' EXIT HUP INT TERM
    {
        printf 'launcher_protocol=%s\n' "$launcher_protocol"
        printf 'binary_version=%s\n' "$binary_version"
        if [ -n "$management_protocol" ]; then
            printf 'management_protocol=%s\n' "$management_protocol"
        fi
    } >"$metadata_tmp"
    chmod 600 "$metadata_tmp"
    mv -f "$metadata_tmp" "$version_dir/COMPATIBILITY"
    trap - EXIT HUP INT TERM
)

tmux_agent_version_management_protocol() (
    version_dir=$1
    metadata_file="$version_dir/COMPATIBILITY"
    [ -f "$metadata_file" ] && [ ! -L "$metadata_file" ] || exit 1
    management=$(awk -F= '
        /^management_protocol=[1-9][0-9]*$/ {
            if (found) exit 1
            found = 1
            print $2
        }
        END { if (!found) exit 1 }
    ' "$metadata_file") || exit 1
    printf '%s\n' "$management"
)

tmux_agent_managed_version_management_capable() (
    candidate_version=$1
    tmux_agent_version_at_least "$candidate_version" "$candidate_version" || exit 1
    data_dir=$(tmux_agent_data_dir)
    versions_dir="$data_dir/versions"
    version_dir="$data_dir/versions/$candidate_version"
    binary_path="$version_dir/tmux-agent"
    [ -d "$versions_dir" ] && [ ! -L "$versions_dir" ] || exit 1
    [ -d "$version_dir" ] && [ ! -L "$version_dir" ] || exit 1
    [ -f "$binary_path" ] && [ ! -L "$binary_path" ] &&
        [ -x "$binary_path" ] || exit 1
    metadata_file="$version_dir/COMPATIBILITY"
    [ -f "$metadata_file" ] && [ ! -L "$metadata_file" ] || exit 1
    metadata=$(tmux_agent_version_compatibility "$version_dir") || exit 1
    contract=$(tmux_agent_compatibility_contract) || exit 1
    [ "${metadata%%|*}" = "${contract%%|*}" ] || exit 1
    [ "${metadata#*|}" = "$candidate_version" ] || exit 1
    [ "$(tmux_agent_version_management_protocol "$version_dir")" = 1 ] || exit 1
    target_file="$version_dir/TARGET"
    [ -f "$target_file" ] && [ ! -L "$target_file" ] || exit 1
    IFS= read -r recorded_target <"$target_file" || exit 1
    [ "$recorded_target" = "$(tmux_agent_target)" ] || exit 1
    tmux_agent_binary_matches "$binary_path" "$candidate_version" || exit 1
)

tmux_agent_manager_binary() (
    data_dir=$(tmux_agent_data_dir)
    manager_path="$data_dir/manager"
    [ -L "$manager_path" ] || exit 1
    manager_target=$(readlink "$manager_path" 2>/dev/null) || exit 1
    case "$manager_target" in
        versions/*)
            manager_suffix=${manager_target#versions/}
            ;;
        "$data_dir"/versions/*)
            manager_suffix=${manager_target#"$data_dir"/versions/}
            ;;
        *) exit 1 ;;
    esac
    case "$manager_suffix" in
        */tmux-agent) manager_version=${manager_suffix%/tmux-agent} ;;
        *) exit 1 ;;
    esac
    case "$manager_version" in
        '' | */*) exit 1 ;;
    esac
    tmux_agent_version_at_least "$manager_version" "$manager_version" || exit 1
    expected_relative="versions/$manager_version/tmux-agent"
    expected_absolute="$data_dir/$expected_relative"
    case "$manager_target" in
        "$expected_relative" | "$expected_absolute") ;;
        *) exit 1 ;;
    esac
    tmux_agent_managed_version_management_capable "$manager_version" || exit 1
    reported_version=$(tmux_agent_binary_version "$manager_path") || exit 1
    [ "$reported_version" = "$manager_version" ] || exit 1
    printf '%s\n' "$manager_path"
)

tmux_agent_is_management_command() {
    expect_config_value=0
    for argument in "$@"; do
        if [ "$expect_config_value" -eq 1 ]; then
            expect_config_value=0
            continue
        fi
        case "$argument" in
            --config)
                expect_config_value=1
                ;;
            --config=*)
                ;;
            update | versions | rollback)
                return 0
                ;;
            -*)
                ;;
            *)
                return 1
                ;;
        esac
    done
    return 1
}

tmux_agent_managed_version_compatible() (
    candidate_version=$1
    data_dir=$(tmux_agent_data_dir)
    version_dir="$data_dir/versions/$candidate_version"
    binary_path="$version_dir/tmux-agent"
    reported_version=$(tmux_agent_binary_version "$binary_path") || exit 1
    [ "$reported_version" = "$candidate_version" ] || exit 1
    contract=$(tmux_agent_compatibility_contract) || exit 1
    required_protocol=${contract%%|*}
    minimum_version=${contract#*|}
    metadata=$(tmux_agent_version_compatibility "$version_dir") || exit 1
    installed_protocol=${metadata%%|*}
    installed_version=${metadata#*|}
    [ "$installed_protocol" = "$required_protocol" ] || exit 1
    [ "$installed_version" = "$reported_version" ] || exit 1
    tmux_agent_version_at_least "$reported_version" "$minimum_version"
)

tmux_agent_current_managed_version() (
    data_dir=$(tmux_agent_data_dir)
    current_path="$data_dir/current"
    [ -x "$current_path" ] || exit 1
    reported_version=$(tmux_agent_binary_version "$current_path") || exit 1
    current_target=$(readlink "$current_path" 2>/dev/null) || exit 1
    expected_relative="versions/$reported_version/tmux-agent"
    expected_absolute="$data_dir/$expected_relative"
    case "$current_target" in
        "$expected_relative" | "$expected_absolute") ;;
        *) exit 1 ;;
    esac
    printf '%s\n' "$reported_version"
)

tmux_agent_current_binary_compatible() (
    current_version=$(tmux_agent_current_managed_version) || exit 1
    tmux_agent_managed_version_compatible "$current_version"
)

tmux_agent_install_lock_acquire() (
    data_dir=$(tmux_agent_data_dir)
    lock_dir="$data_dir/.install.lock"
    attempts=${TMUX_AGENT_INSTALL_LOCK_ATTEMPTS:-300}
    incomplete_grace_attempts=${TMUX_AGENT_INCOMPLETE_LOCK_GRACE_ATTEMPTS:-50}
    case "$attempts" in
        '' | *[!0-9]*)
            printf '%s\n' 'tmux-agent: invalid installation lock attempt count' >&2
            exit 1
            ;;
    esac
    case "$incomplete_grace_attempts" in
        '' | 0 | *[!0-9]*)
            printf '%s\n' 'tmux-agent: invalid incomplete lock grace count' >&2
            exit 1
            ;;
    esac
    attempt=0
    while ! mkdir "$lock_dir" 2>/dev/null; do
        attempt=$((attempt + 1))
        lock_pid=
        if [ -r "$lock_dir/pid" ]; then
            IFS= read -r lock_pid <"$lock_dir/pid" || true
        fi
        case "$lock_pid" in
            '' | *[!0-9]*) lock_pid= ;;
        esac
        if [ -n "$lock_pid" ] && ! kill -0 "$lock_pid" 2>/dev/null; then
            rm -f -- "$lock_dir/pid"
            rmdir "$lock_dir" 2>/dev/null || true
            continue
        fi
        if [ -z "$lock_pid" ] && [ "$attempt" -ge "$incomplete_grace_attempts" ]; then
            rm -f -- "$lock_dir/pid"
            rmdir "$lock_dir" 2>/dev/null || true
            continue
        fi
        if [ "$attempt" -ge "$attempts" ]; then
            printf '%s\n' 'tmux-agent: timed out waiting for the installation lock' >&2
            exit 1
        fi
        sleep 0.1
    done
    if ! printf '%s\n' "$$" >"$lock_dir/pid" ||
        ! chmod 600 "$lock_dir/pid"; then
        rm -f -- "$lock_dir/pid"
        rmdir "$lock_dir" 2>/dev/null || true
        printf '%s\n' 'tmux-agent: failed to publish installation lock owner' >&2
        exit 1
    fi
)

tmux_agent_install_lock_release() (
    lock_dir="$(tmux_agent_data_dir)/.install.lock"
    lock_pid=
    if [ -r "$lock_dir/pid" ]; then
        IFS= read -r lock_pid <"$lock_dir/pid" || true
    fi
    [ "$lock_pid" = "$$" ] || exit 1
    rm -f -- "$lock_dir/pid"
    rmdir "$lock_dir" 2>/dev/null
)

tmux_agent_activate_managed_link() (
    link_name=$1
    candidate_version=$2
    data_dir=$(tmux_agent_data_dir)
    lock_pid=
    if [ -r "$data_dir/.install.lock/pid" ]; then
        IFS= read -r lock_pid <"$data_dir/.install.lock/pid" || true
    fi
    [ "$lock_pid" = "$$" ] || exit 1
    candidate_binary="$data_dir/versions/$candidate_version/tmux-agent"
    tmux_agent_binary_matches "$candidate_binary" "$candidate_version" || exit 1
    case "$link_name" in
        current)
            ;;
        manager)
            tmux_agent_managed_version_management_capable "$candidate_version" || exit 1
            ;;
        *) exit 1 ;;
    esac
    link_tmp="$data_dir/.${link_name}.$$"
    trap 'rm -f -- "$link_tmp"' EXIT HUP INT TERM
    ln -s "versions/$candidate_version/tmux-agent" "$link_tmp"
    mv -f "$link_tmp" "$data_dir/$link_name"
    trap - EXIT HUP INT TERM
)

tmux_agent_activate_version() {
    tmux_agent_activate_managed_link current "$1"
}

tmux_agent_activate_manager() {
    tmux_agent_activate_managed_link manager "$1"
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
