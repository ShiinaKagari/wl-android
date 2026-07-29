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

        touchForwarder = TouchForwarder { id, x, y, phase, timeMs ->
            if (nativeHandle != 0L) {
                NativeBridge.nativeOnTouch(nativeHandle, id, x, y, phase, timeMs)
            }
        }
        surfaceView.setOnTouchListener { _, event ->
            touchForwarder.handle(event)
            true
        }

        collector = ScreenInfoCollector(this) { w, h, ref, dpi ->
            touchForwarder.screenWidth = w
            touchForwarder.screenHeight = h
            if (nativeHandle != 0L) {
                NativeBridge.nativeOnConfig(nativeHandle, w, h, ref, dpi)
            }
        }

        // Connect once, keep alive across lifecycle
        nativeHandle = NativeBridge.nativeInit(socketPath)
    }

    override fun onResume() {
        super.onResume()
        collector.start()
    }

    override fun onPause() {
        collector.stop()
        super.onPause()
    }

    override fun onDestroy() {
        NativeBridge.nativeDestroy(nativeHandle)
        nativeHandle = 0
        super.onDestroy()
    }

    private val surfaceCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            if (nativeHandle != 0L) {
                NativeBridge.nativeSetSurface(nativeHandle, holder.surface)
            }
        }

        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            collector.emit()
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            if (nativeHandle != 0L) {
                NativeBridge.nativeSetSurface(nativeHandle, null)
            }
        }
    }
}
