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

case "${1:-}" in
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
    *) echo "usage: $0 prepare|restore|probe" >&2; exit 2 ;;
esac
