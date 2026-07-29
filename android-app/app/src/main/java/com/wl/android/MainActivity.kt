package com.wl.android

import android.app.Activity
import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.MotionEvent
import android.view.WindowManager

class MainActivity : Activity() {
    private lateinit var surfaceView: SurfaceView
    private lateinit var collector: ScreenInfoCollector
    private lateinit var touchForwarder: TouchForwarder
    private var nativeHandle: Long = 0
    private val socketPath = "/data/local/tmp/wl-android/land.sock"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        surfaceView = SurfaceView(this).apply {
            holder.addCallback(surfaceCallback)
        }
        setContentView(surfaceView)
        surfaceView.setOnTouchListener { _, event ->
            touchForwarder.handle(event)
            true
        }

        collector = ScreenInfoCollector(this) { w, h, ref, dpi ->
            if (nativeHandle != 0L) {
                NativeBridge.nativeOnConfig(nativeHandle, w, h, ref, dpi)
            }
        }

        touchForwarder = TouchForwarder { id, x, y, phase, timeMs ->
            if (nativeHandle != 0L) {
                NativeBridge.nativeOnTouch(nativeHandle, id, x, y, phase, timeMs)
            }
        }
    }

    override fun onResume() {
        super.onResume()
        collector.start()
        nativeHandle = NativeBridge.nativeInit(socketPath)

        if (surfaceView.holder.surface.isValid) {
            NativeBridge.nativeSetSurface(nativeHandle, surfaceView.holder.surface)
        }
    }

    override fun onPause() {
        collector.stop()
        NativeBridge.nativeSetSurface(nativeHandle, null)
        NativeBridge.nativeDestroy(nativeHandle)
        nativeHandle = 0
        super.onPause()
    }

    private val surfaceCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            if (nativeHandle != 0L) {
                NativeBridge.nativeSetSurface(nativeHandle, holder.surface)
            }
        }

        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            // re-emit config on surface change (rotation)
            collector.emit()
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            NativeBridge.nativeSetSurface(nativeHandle, null)
        }
    }
}
