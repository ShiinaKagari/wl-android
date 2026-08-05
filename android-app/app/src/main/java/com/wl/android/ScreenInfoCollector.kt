package com.wl.android

import android.app.Activity
import android.hardware.display.DisplayManager
import android.os.SystemClock

/** Reports the physical display's current mode (resolution + refresh rate) as
 * the output mode, and the scaled render target resolution to the backend.
 * The scale factor (user-set, default 1.0) multiplies the physical size to
 * produce the render resolution KWin renders at; the App's SurfaceView stays
 * fullscreen, so SurfaceFlinger scales the smaller render buffer up. */
class ScreenInfoCollector(
    private val activity: Activity,
    private val onConfig: (Int, Int, Int, Int, Int) -> Unit
) {
    private val dm = activity.getSystemService(DisplayManager::class.java)
    private var lastSent = 0L

    /** User-set render scale, default 1.0 (no scaling). Values like 0.5, 1.5, 2.0. */
    @Volatile var scale = 1.0f
        set(value) {
            field = if (value > 0f) value else 1.0f
        }

    /** Frame pacing mode: 0 free, 1 vsync-align, 2 performance, 3 power-save. */
    @Volatile var frameMode = 0

    fun start() {
        dm?.registerDisplayListener(listener, null)
        emit()
    }

    fun stop() {
        dm?.unregisterDisplayListener(listener)
    }

    fun emit() {
        // Physical display mode: currentMode gives the real resolution and
        // refresh rate of the panel (more accurate than displayMetrics, which
        // can lag behind mode switches).
        val mode = activity.display?.mode
        val physW: Int
        val physH: Int
        val refresh: Float
        if (mode != null && mode.physicalWidth > 0 && mode.physicalHeight > 0) {
            physW = mode.physicalWidth
            physH = mode.physicalHeight
            refresh = mode.refreshRate
        } else {
            val metrics = activity.resources.displayMetrics
            physW = metrics.widthPixels
            physH = metrics.heightPixels
            refresh = activity.display?.refreshRate ?: 60f
        }
        val dpi = activity.resources.displayMetrics.densityDpi
        // Render target resolution = physical × scale (user-controlled).
        val rw = (physW * scale).toInt().coerceAtLeast(1)
        val rh = (physH * scale).toInt().coerceAtLeast(1)
        onConfig(rw, rh, (refresh * 1000).toInt(), dpi, frameMode)
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
