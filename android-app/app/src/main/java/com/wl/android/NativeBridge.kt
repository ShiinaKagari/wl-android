package com.wl.android

import android.view.Surface

object NativeBridge {
    init {
        System.loadLibrary("land_native")
    }

    external fun nativeInit(socketPath: String): Long
    external fun nativeSetSurface(handle: Long, surface: Surface?)
    external fun nativeOnConfig(handle: Long, w: Int, h: Int, refreshMilliHz: Int, dpi: Int)
    external fun nativeOnTouch(handle: Long, id: Int, x: Float, y: Float, phase: Int, timeMs: Int)
    external fun nativeOnKey(handle: Long, keycode: Int, state: Int, timeMs: Int)
    external fun nativeGetState(handle: Long): Int
    external fun nativeGetSocketFd(handle: Long): Int
    external fun nativeDestroy(handle: Long)
}
