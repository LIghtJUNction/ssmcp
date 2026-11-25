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

echo 'SSMCP 操作菜单'
echo '查看状态 / 启动服务 / 退出'
echo '音量＋切换，音量−确认。20 秒未按音量键会自动退出。'

umask 077
work=$(mktemp -d /data/local/tmp/ssmcp-action.XXXXXX) || exit 1
cleanup() {
    if [ -n "${reader:-}" ]; then
        kill "$reader" 2>/dev/null
        wait "$reader" 2>/dev/null
    fi
    rm -f "$work/keys"
    rmdir "$work"
}
reader=
trap cleanup 0
trap 'exit 0' HUP INT TERM
mkfifo "$work/keys" || exit 1
/system/bin/getevent -ql >"$work/keys" 2>/dev/null &
reader=$!
exec 3<"$work/keys"

choice=查看状态
while :; do
    printf '当前选择：%s\n' "$choice"
    key=$(/system/bin/timeout 20 /system/bin/sh -c '
        while read -r device type code value; do
            case "$type:$code:$value" in
                EV_KEY:KEY_VOLUMEUP:DOWN) echo next; exit 0 ;;
                EV_KEY:KEY_VOLUMEDOWN:DOWN) echo confirm; exit 0 ;;
            esac
        done
        exit 1
    ' <&3) || exit 0
    if [ "$key" = confirm ]; then break; fi
    case "$choice" in
        查看状态) choice=启动服务 ;;
        启动服务) choice=退出 ;;
        退出) choice=查看状态 ;;
    esac
done
exec 3<&-
cleanup
trap - 0 HUP INT TERM

case "$choice" in
    查看状态)
        if read_console_url; then echo '本机服务已运行。'; else echo '本机服务未运行。'; fi ;;
    启动服务)
        if read_console_url; then echo '本机服务已运行。'
        elif start_console; then echo '已请求启动本机服务。'
        else echo '本机服务启动失败。'; exit 1; fi ;;
esac
