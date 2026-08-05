package com.wl.android

import android.view.Surface

/** CONN-STATE: notified by native on connection state changes (event-driven,
 * replaces the previous status polling). */
interface StatusListener {
    fun onStateChanged(state: Int)
}

object NativeBridge {
    init {
        System.loadLibrary("land_native")
    }

    external fun nativeInit(socketPath: String): Long
    external fun nativeSetSurface(handle: Long, surface: Surface?)
    external fun nativeSetRenderSize(handle: Long, w: Int, h: Int)
    external fun nativeSetStatusListener(handle: Long, listener: StatusListener?)
    external fun nativeOnConfig(handle: Long, w: Int, h: Int, refreshMilliHz: Int, dpi: Int, frameMode: Int)
    external fun nativeOnTouch(handle: Long, id: Int, x: Float, y: Float, phase: Int, timeMs: Int)
    external fun nativeOnKey(handle: Long, keycode: Int, state: Int, timeMs: Int)
    external fun nativeDestroy(handle: Long)
}
