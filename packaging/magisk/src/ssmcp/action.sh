#!/system/bin/sh
MODDIR=${0%/*}
# shellcheck disable=SC1091
. "$MODDIR/launch.sh" || exit 1

# Older callers used this argument to ensure the service was running.
if [ "$#" -ne 0 ]; then
    [ "$#" = 1 ] && [ "$1" = setup ] || exit 2
    start_console
    exit $?
fi

start_console && open_console gui
