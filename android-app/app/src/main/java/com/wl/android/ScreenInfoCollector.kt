package com.wl.android

import android.app.Activity
import android.hardware.display.DisplayManager
import android.os.SystemClock

class ScreenInfoCollector(
    private val activity: Activity,
    private val onConfig: (Int, Int, Int, Int) -> Unit
) {
    private val dm = activity.getSystemService(DisplayManager::class.java)
    private var lastSent = 0L

    fun start() {
        dm?.registerDisplayListener(listener, null)
        emit()
    }

    fun stop() {
        dm?.unregisterDisplayListener(listener)
    }

    fun emit() {
        val metrics = activity.resources.displayMetrics
        val refresh = activity.display?.refreshRate ?: 60f
        val dpi = metrics.densityDpi
        onConfig(metrics.widthPixels, metrics.heightPixels, (refresh * 1000).toInt(), dpi)
    }

    private val listener = object : DisplayManager.DisplayListener {
        override fun onDisplayChanged(displayId: Int) {
            val now = SystemClock.elapsedRealtime()
            if (now - lastSent > 200) {
                lastSent = now
                emit()
            }
        }
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
    }
}
