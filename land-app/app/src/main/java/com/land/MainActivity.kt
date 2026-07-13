package com.land

import android.os.Bundle
import android.view.Choreographer
import android.view.SurfaceView
import android.view.SurfaceHolder
import android.view.MotionEvent
import androidx.appcompat.app.AppCompatActivity

class MainActivity : AppCompatActivity(), SurfaceHolder.Callback, Choreographer.FrameCallback {

    private lateinit var surfaceView: SurfaceView
    private var initialized = false
    private var surfaceReady = false
    private var pendingFd: Int? = null
    private var pendingWidth: Int = 0
    private var pendingHeight: Int = 0
    private var choreographer: Choreographer? = null
    private var framePending = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        surfaceView = findViewById(R.id.surfaceView)
        surfaceView.holder.addCallback(this)
        surfaceView.setOnTouchListener { _, event -> TouchForwarder.onTouch(event) }

        choreographer = Choreographer.getInstance()

        nativeInit()
        initialized = true
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        val surface = holder.surface
        val width = surfaceView.width
        val height = surfaceView.height
        nativeSetSurface(surface, width, height)
        surfaceReady = true

        pendingFd?.let { fd ->
            queueFrame(fd, pendingWidth, pendingHeight)
            pendingFd = null
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        nativeSetSurface(holder.surface, width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady = false
    }

    override fun onDestroy() {
        super.onDestroy()
        initialized = false
        nativeDestroy()
    }

    /** 由 native 层调用: 有新的 DMA-BUF 帧到达 */
    fun onFrameReceived(fd: Int, width: Int, height: Int) {
        if (surfaceReady) {
            queueFrame(fd, width, height)
        } else {
            pendingFd = fd
            pendingWidth = width
            pendingHeight = height
        }
    }

    /** 将帧加入渲染队列，等待下一个 vsync */
    private fun queueFrame(fd: Int, width: Int, height: Int) {
        pendingFd = fd
        pendingWidth = width
        pendingHeight = height
        if (!framePending) {
            framePending = true
            choreographer?.postFrameCallback(this)
        }
    }

    /** Choreographer vsync 回调：在 vsync 时刻渲染帧 */
    override fun doFrame(frameTimeNanos: Long) {
        framePending = false
        pendingFd?.let { fd ->
            nativeRenderFrame(fd, pendingWidth, pendingHeight)
            pendingFd = null
        }
    }

    @dalvik.annotation.optimization.FastNative
    private external fun nativeInit()

    @dalvik.annotation.optimization.FastNative
    private external fun nativeSetSurface(surface: android.view.Surface, width: Int, height: Int)

    @dalvik.annotation.optimization.FastNative
    private external fun nativeRenderFrame(fd: Int, width: Int, height: Int)

    @dalvik.annotation.optimization.FastNative
    private external fun nativeDestroy()

    companion object {
        init {
            System.loadLibrary("land-native")
            System.loadLibrary("land-bridge")
        }
    }
}
