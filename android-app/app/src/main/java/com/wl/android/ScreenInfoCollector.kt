package com.wl.android

import android.app.Activity
import android.hardware.display.DisplayManager
import android.os.SystemClock

/** Reports the physical display's current mode (resolution + refresh rate) as
 * the output mode, and the scaled render target resolution to the backend.
 * The scale factor (user-set, default 1.0) multiplies the physical size to
 * produce the render resolution KWin renders at; the App's SurfaceView stays
 * fullscreen, so SurfaceFlinger scales the smaller render buffer up.
 *
 * CONF TIMING (event-driven, foreground-only):
 * - CONF is emitted only while the App is FOREGROUND: on foreground entry
 *   (onResume) and on real display changes (rotation / inset-driven surface
 *   changes / user panel toggles). Background display churn (other apps
 *   changing refresh rate, UI transitions) never touches the server.
 * - Going BACKGROUND emits one CONF with frame_mode=3 (power-save) so the
 *   server quarters the vsync beat (144Hz → ~36Hz) and KWin stops burning
 *   frames nobody displays. Returning foreground emits frame_mode=0 (full
 *   rate).
 * - VALUE GUARD: emit() fires only when the reported tuple actually changed.
 *   The panel is LTPO — the current Display.Mode refresh rate dances between
 *   60/120/144Hz; re-reporting every hop made the server update wl_output
 *   mode repeatedly, rebuilding KWin's output and freezing the picture.
 *   Refresh comes from the PREFERRED mode (panel's native rate, stable).
 * - The activity is locked to landscape + non-resizable, so displayMetrics
 *   never jumps to a portrait size mid-session. */
class ScreenInfoCollector(
    private val activity: Activity,
    private val onConfig: (Int, Int, Int, Int, Int) -> Unit
) {
    private val dm = activity.getSystemService(DisplayManager::class.java)
    private var lastSent = 0L
    private var lastKey: String? = null

    /** True while the App is foregrounded (onResume..onPause). Display
     * changes are only reported in this window. */
    @Volatile private var foreground = false

    /** User-set render scale, default 1.0 (no scaling). Values like 0.5, 1.5, 2.0. */
    @Volatile var scale = 1.0f
        set(value) {
            field = if (value > 0f) value else 1.0f
        }

    /** Frame pacing mode: 0 free, 1 vsync-align, 2 performance, 3 power-save. */
    @Volatile var frameMode = 0

    /** App entered foreground: start listening, report full-rate config. */
    fun onAppForeground() {
        foreground = true
        frameMode = 0
        dm?.registerDisplayListener(listener, null)
        emit()
    }

    /** App left foreground: stop listening, drop to power-save for the backend. */
    fun onAppBackground() {
        foreground = false
        dm?.unregisterDisplayListener(listener)
        frameMode = 3 // power-save: server quarters the vsync beat
        emit()
    }

    fun emit() {
        // Refresh rate: the panel's TOP supported mode rate (native, stable) —
        // the current Display.Mode is VRR-aware and dances on LTPO panels,
        // which would re-report CONF on every refresh hop and disturb KWin.
        // (preferredDisplayModeId was removed from Display in android-36;
        // supportedModes max refresh is the panel's native ceiling.)
        val display: android.view.Display? =
            dm?.getDisplay(android.view.Display.DEFAULT_DISPLAY)
        val mode = display?.supportedModes?.maxByOrNull { it.refreshRate }
            ?: display?.mode
        val refresh: Float = if (mode != null && mode.refreshRate > 0f) {
            mode.refreshRate
        } else {
            display?.refreshRate ?: 60f
        }
        // Resolution: displayMetrics is ALREADY rotated to the App's current
        // orientation (the SurfaceView buffer is 3392x2400 landscape while the
        // panel's Display.Mode reports 2400x3392 portrait). Using the panel
        // size here would hand KWin a portrait render target while the App
        // presents landscape — a 90° mismatch. Metrics match the surface.
        // (Activity is locked landscape + non-resizable, so this is stable.)
        val metrics = activity.resources.displayMetrics
        val physW = metrics.widthPixels
        val physH = metrics.heightPixels
        val dpi = metrics.densityDpi
        // Render target resolution = physical × scale (user-controlled).
        val rw = (physW * scale).toInt().coerceAtLeast(1)
        val rh = (physH * scale).toInt().coerceAtLeast(1)
        // VALUE GUARD: only report on an actual change (scale/frameMode
        // included — user panel toggles and foreground/background transitions
        // must still go through).
        val key = "$rw,$rh,$refresh,$dpi,$frameMode"
        if (key == lastKey) {
            return
        }
        lastKey = key
        onConfig(rw, rh, (refresh * 1000).toInt(), dpi, frameMode)
    }

    private val listener = object : DisplayManager.DisplayListener {
        override fun onDisplayChanged(displayId: Int) {
            // Foreground-only: background display churn must not reach KWin.
            if (!foreground) {
                return
            }
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
