#!/usr/bin/env bash
# Prepare the job's ephemeral Linux runner for real client admission tests.
set -euo pipefail
uid=$(id -u)
user=$(id -un)
unit="user@$uid.service"
export XDG_RUNTIME_DIR="/run/user/$uid"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

if [[ ${1:-} = check-kernel ]]; then
    root="/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)"
    read -r quota period < "$root/cpu.max"
    [[ $quota =~ ^[0-9]+$ && $period =~ ^[0-9]+$ ]] && (( quota * 4 == period )) || exit 1
    test "$(cat "$root/memory.max")" = 67108864
    test "$(cat "$root/memory.swap.max")" = 0
    mkdir "$root/supervisor"
    echo 0 > "$root/supervisor/cgroup.procs"
    echo +memory > "$root/cgroup.subtree_control"
    test -f "$root/supervisor/memory.max"
    exit
fi

probe() {
    timeout 20s systemd-run --user --scope --quiet --collect \
        -p CPUQuota=25% -p MemoryMax=64M -p MemorySwapMax=0 -p Delegate=memory \
        -- /bin/bash "$(realpath "$0")" check-kernel
}

if [[ ${1:-} = probe ]]; then probe; exit; fi
: "${RUNNER_TEMP:?hosted runner temp directory required}"
test "${RUNNER_ENVIRONMENT:-}" = github-hosted
state="$RUNNER_TEMP/ssmcp-manager"
dropin="/run/systemd/system/$unit.d/95-ssmcp-ci-delegation.conf"

# A new manager reads Delegate= at first start. Jammy's already running
# hosted runner manager may have only memory/pids and can reject a restart.
release_smoke() (
    local binary
    binary=$(realpath "$1")
    test -x "$binary"
    if probe >&2; then
        exec "$binary" --list-tools --shell /bin/sh
    fi

    local work account smoke_uid smoke_unit='' override=''
    work=$(mktemp -d /tmp/ssmcp-release-smoke.XXXXXXXX)
    account="ssmcp-ci-${BASHPID}-${RANDOM}"
    local added=false configured=false new_dir=false linger=false
    # Invoked on both success and failure by the EXIT trap below.
    # shellcheck disable=SC2329
    cleanup() {
        local status=$?
        trap - EXIT
        set +e
        if $linger; then sudo -n loginctl disable-linger "$account" >&2 || status=1; fi
        if [[ -n $smoke_unit ]]; then
            timeout 30s sudo -n systemctl stop "$smoke_unit" >&2 || status=1
        fi
        if $configured; then
            sudo -n rm -f -- "$override" || status=1
            if $new_dir; then sudo -n rmdir -- "$(dirname "$override")" || status=1; fi
            sudo -n systemctl daemon-reload >&2 || status=1
        fi
        if $added; then sudo -n userdel "$account" >&2 || status=1; fi
        sudo -n rm -rf -- "$work" || status=1
        exit "$status"
    }
    trap cleanup EXIT
    # Only this disposable UID can read its home; the enclosing directory
    # permits traversal to the immutable binary and probe copied by the runner.
    chmod 0711 "$work"
    install -m 0755 "$binary" "$work/ssmcp"
    install -m 0755 "$0" "$work/check.sh"
    sudo -n useradd --user-group --no-create-home --home-dir "$work/home" --shell /bin/bash "$account"
    added=true
    smoke_uid=$(id -u "$account")
    smoke_unit="user@$smoke_uid.service"
    sudo -n install -d -m 0700 -o "$account" -g "$account" "$work/home"
    override="/run/systemd/system/$smoke_unit.d/95-ssmcp-release-delegation.conf"
    test ! -e "$override"
    if [[ ! -d $(dirname "$override") ]]; then new_dir=true; fi
    sudo -n install -d -m 0755 "$(dirname "$override")"
    printf '%s\n' '[Service]' 'Delegate=cpu memory pids' | sudo -n tee "$override" >/dev/null
    configured=true
    sudo -n systemctl daemon-reload
    sudo -n loginctl enable-linger "$account"
    linger=true
    if ! timeout 30s sudo -n systemctl start "$smoke_unit"; then
        sudo -n journalctl -b -u "$smoke_unit" -n 20 --no-pager >&2 || :
        return 1
    fi
    local -a as_manager=(sudo -n -H -u "$account" env "XDG_RUNTIME_DIR=/run/user/$smoke_uid"
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$smoke_uid/bus" systemd-run
        --user --wait --pipe --quiet --collect "--working-directory=$work/home")
    # Start the probe and then the exact candidate from inside this user's
    # manager, preserving all ancestor restrictions when the client is admitted.
    "${as_manager[@]}" /bin/bash "$work/check.sh" probe >&2
    "${as_manager[@]}" "$work/ssmcp" --list-tools --shell /bin/sh
)

case "${1:-}" in
    release-smoke)
        test "$#" -eq 2
        release_smoke "$2"
        ;;
    prepare)
        mkdir -p "$state"
        if ! timeout 5s systemctl --user show --property=Version > /dev/null; then
            if [[ $(timeout 5s loginctl show-user "$user" --property=Linger --value 2>/dev/null || :) != yes ]] &&
                timeout 10s sudo -n loginctl enable-linger "$user"; then
                touch "$state/linger"
            fi
            timeout 20s sudo -n systemctl start "$unit"
        fi
        group=$(systemctl show "$unit" -p ControlGroup --value)
        test -n "$group" && test -r "/sys/fs/cgroup$group/cgroup.controllers"
        if ! probe; then
            # Older distro defaults can delegate only memory and pids to a user.
            # This drop-in belongs only to the ephemeral VM and this runner UID.
            sudo -n install -d -m 0755 "$(dirname "$dropin")"
            test ! -e "$dropin"
            printf '%s\n' '[Service]' 'Delegate=cpu memory pids' | sudo -n tee "$dropin" >/dev/null
            touch "$state/dropin"
            sudo -n systemctl daemon-reload
            if ! probe; then
                if grep -Fq "0::$group/" /proc/self/cgroup; then
                    echo "Cannot restart the user manager containing the CI runner" >&2
                    exit 1
                fi
                timeout 30s sudo -n systemctl restart "$unit"
                probe
            fi
        fi
        printf 'XDG_RUNTIME_DIR=%s\nDBUS_SESSION_BUS_ADDRESS=%s\n' \
            "$XDG_RUNTIME_DIR" "$DBUS_SESSION_BUS_ADDRESS" >> "$GITHUB_ENV"
        ;;
    restore)
        if [[ -f $state/dropin ]]; then
            sudo -n rm "$dropin"
            sudo -n systemctl daemon-reload
            rm "$state/dropin"
        fi
        if [[ -f $state/linger ]]; then
            timeout 20s sudo -n loginctl disable-linger "$user"
            rm "$state/linger"
        fi
        ;;
    *) echo "usage: $0 prepare|restore|probe|release-smoke EXECUTABLE" >&2; exit 2 ;;
esac
