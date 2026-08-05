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
        // Refresh rate: Display.Mode gives the panel's real current mode rate
        // (VRR-aware, more accurate than the deprecated refreshRate getter).
        val mode = activity.display?.mode
        val refresh: Float = if (mode != null && mode.refreshRate > 0f) {
            mode.refreshRate
        } else {
            activity.display?.refreshRate ?: 60f
        }
        // Resolution: displayMetrics is ALREADY rotated to the App's current
        // orientation (the SurfaceView buffer is 3392x2400 landscape while the
        // panel's Display.Mode reports 2400x3392 portrait). Using the panel
        // size here would hand KWin a portrait render target while the App
        // presents landscape — a 90° mismatch. Metrics match the surface.
        val metrics = activity.resources.displayMetrics
        val physW = metrics.widthPixels
        val physH = metrics.heightPixels
        val dpi = metrics.densityDpi
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
