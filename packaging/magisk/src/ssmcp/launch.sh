#!/system/bin/sh
# Sourced by Magisk lifecycle hooks.
state=/data/adb/ssmcp
port=42387
prepare_console_state() {
    umask 077
    for dir in "$state" "$state/home" "$state/sessions"; do
        [ ! -L "$dir" ] && mkdir -p "$dir" && chmod 0700 "$dir" || return 1
    done
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
    case "$url" in "http://127.0.0.1:$port/#"*) credential=${url#*#} ;; *) return 1 ;; esac
    [ "${#credential}" -eq 64 ] || return 1
    case "$credential" in *[!0-9a-f]*) return 1 ;; esac
}
start_console() {
    [ ! -e "$MODDIR/disable" ] && [ ! -e "$MODDIR/remove" ] || return 1
    read_console_url && return
    prepare_console_state || return 1
    HOME="$state/home" SSMCP_SESSION_HOME="$state/sessions" TMPDIR="$state" SHELL=/system/bin/sh PATH="/system/bin:/system/xbin:$state/home/.local/bin" \
        "$MODDIR/bin/ssmcp" __magisk "$state" "$port" </dev/null >>"$state/console.log" 2>&1 &
}
# The native listener validates the target before starting a session. This
# additional check only decides whether the owner should see setup or the GUI.
has_confirmed_target() {
    [ -s "$state/root-consent-v1" ] || return 1
    target=
    while read -r command arg extra || [ -n "$command" ]; do
        case "$command" in ''|'#'*) continue ;; esac
        [ "$command" = ssmcp ] && [ -n "$arg" ] && [ -z "$extra" ] && [ -z "$target" ] || return 1
        target=$arg
    done <"$state/ssmcp.rc"
    [ -n "$target" ] && [ "$target" = "$(cat "$state/root-consent-v1")" ]
}
open_console() {
    case "${1:-gui}" in setup|gui) ;; *) return 1 ;; esac
    tries=0
    while [ "$tries" -lt 15 ]; do
        if read_console_url; then
            if [ "${1:-gui}" = setup ] || ! has_confirmed_target; then
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
