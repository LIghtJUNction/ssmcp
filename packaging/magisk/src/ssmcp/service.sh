#!/system/bin/sh
MODDIR=${0%/*}
# shellcheck disable=SC1091
. "$MODDIR/launch.sh" || exit 1
start_console
