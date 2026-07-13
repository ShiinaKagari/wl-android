#!/system/bin/sh
ui_print "- wl-android Socket Daemon"
ui_print "- Manages /dev/socket/land.sock lifecycle"
ui_print ""
set_perm_recursive $MODPATH/bin 0 0 0755 0755
