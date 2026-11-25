#!/system/bin/sh
# Sourced by Magisk lifecycle hooks.
state=/data/adb/ssmcp
port=42387
prepare_console_state() {
    umask 077
    mkdir -p "$state/home" && chmod 0700 "$state" "$state/home" || return 1
    if [ ! -e "$state/ssmcp.rc" ]; then
        cp "$MODDIR/ssmcp.rc" "$state/ssmcp.rc" || return 1
    fi
}
read_console_url() {
    [ -s "$state/console.url" ] || return 1
    {
        IFS= read -r pid && IFS= read -r url
    } <"$state/console.url" || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    [ "/proc/$pid/exe" -ef "$MODDIR/bin/ssmcp" ] || return 1
    case "$url" in "http://127.0.0.1:$port/#"*) return 0 ;; *) return 1 ;; esac
}
start_console() {
    read_console_url && return
    prepare_console_state || return 1
    HOME="$state/home" TMPDIR="$state" SHELL=/system/bin/sh PATH=/system/bin:/system/xbin \
        "$MODDIR/bin/ssmcp" __magisk "$state" "$port" </dev/null >>"$state/console.log" 2>&1 &
}
open_console() {
    case "${1:-terminal}" in setup|terminal) ;; *) return 1 ;; esac
    tries=0
    while [ "$tries" -lt 15 ]; do
        if read_console_url; then
            if [ "${1:-terminal}" = setup ]; then
                url="http://127.0.0.1:$port/setup#${url##*#}"
            fi
            /system/bin/am start -a android.intent.action.VIEW -d "$url" >/dev/null 2>&1
            return
        fi
        tries=$((tries + 1))
        sleep 1
    done
    return 1
}
