#!/system/bin/sh
# wl-android Socket Infrastructure 安装脚本

ui_print "- wl-android Socket Infrastructure v0.1.0"
ui_print "- Creates /dev/socket/land.sock directory"
ui_print "- Applies SELinux rules for socket access"
ui_print ""
ui_print "- Note: This module does NOT include landd daemon."
ui_print "- Install land-app APK separately for full functionality."
