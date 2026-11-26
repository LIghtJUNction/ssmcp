#!/system/bin/sh
[ "$ARCH" = arm64 ] && [ "$API" -ge 26 ] || abort "Requires Android 8+ (arm64-v8a)."
set_perm "$MODPATH/bin/ssmcp" 0 0 0755
if [ "$BOOTMODE" = true ] && [ ! -e /data/adb/ssmcp/ssmcp.rc ] && [ ! -s /data/adb/ssmcp/root-consent-v1 ]; then
    (
        MODDIR=$MODPATH
        # shellcheck disable=SC1091
        . "$MODDIR/launch.sh" || exit 1
        start_console && open_console gui
    ) || ui_print "- 重启后可从模块管理器打开 SSMCP 工作台。"
fi
ui_print "- 打开模块 WebUI，填写用户名和服务器地址，点击「连接」即可聊天。"
