#!/system/bin/sh
# Called only when the owner explicitly opens the installation guide.
MODDIR=${0%/onboarding/open.sh}
. "$MODDIR/launch.sh" || exit 1
start_console && open_console setup
