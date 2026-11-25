#!/system/bin/sh
[ "$ARCH" = arm64 ] && [ "$API" -ge 26 ] || abort "Requires Android 8+ (arm64-v8a)."
set_perm "$MODPATH/bin/ssmcp" 0 0 0755
MODDIR=$MODPATH
# shellcheck disable=SC1091
. "$MODDIR/launch.sh" || abort "Cannot initialize /data/adb/ssmcp."
if [ "$BOOTMODE" = true ] && [ ! -d /data/adb/modules/ssmcp ]; then
    start_console
    open_console || ui_print "- Open SSMCP from its Magisk action after reboot."
fi
ui_print "- Configure username@server in the SSMCP setup page before connecting."
ui_print "- 连接前请在 SSMCP 配置页填写 用户名@服务器域名。"
