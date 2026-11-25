#!/system/bin/sh
# Sourced by Magisk lifecycle hooks.
state=/data/adb/ssmcp
port=42387
umask 077
mkdir -p "$state/home" || exit 1
chmod 0700 "$state" "$state/home" || exit 1
if [ ! -e "$state/ssmcp.rc" ]; then
    cp "$MODDIR/ssmcp.rc" "$state/ssmcp.rc" || exit 1
fi
export HOME="$state/home" TMPDIR="$state" SHELL=/system/bin/sh
export PATH=/system/bin:/system/xbin
read_console_url() {
    [ -s "$state/console.url" ] || return 1
    {
        IFS= read -r pid && IFS= read -r url
    } <"$state/console.url" || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    [ "$(readlink "/proc/$pid/exe")" = "$MODDIR/bin/ssmcp" ] || return 1
    case "$url" in "http://127.0.0.1:$port/#"*) return 0 ;; *) return 1 ;; esac
}
start_console() {
    read_console_url && return
    "$MODDIR/bin/ssmcp" __magisk "$state" "$port" </dev/null >>"$state/console.log" 2>&1 &
}
open_console() {
    tries=0
    while [ "$tries" -lt 15 ]; do
        if read_console_url; then
            /system/bin/am start -a android.intent.action.VIEW -d "$url" >/dev/null 2>&1
            return
        fi
        tries=$((tries + 1))
        sleep 1
    done
    return 1
}
