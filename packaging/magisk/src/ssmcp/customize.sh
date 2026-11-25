#!/system/bin/sh
[ "$ARCH" = arm64 ] && [ "$API" -ge 26 ] || abort "Requires Android 8+ (arm64-v8a)."
set_perm "$MODPATH/bin/ssmcp" 0 0 0755
MODDIR=$MODPATH
# shellcheck disable=SC1091
. "$MODDIR/launch.sh" || abort "Cannot initialize /data/adb/ssmcp."
if [ "$BOOTMODE" = true ] && [ ! -s "$state/root-consent-v1" ]; then
    start_console
    open_console setup || ui_print "- Open SSMCP from the module manager after reboot to finish setup."
fi
ui_print "- 在安装引导中完成配置。日常使用请打开模块的 WebUI 或操作按钮。"
