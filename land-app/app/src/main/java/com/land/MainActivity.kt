package com.land

import android.os.Bundle
import android.view.Choreographer
import android.view.SurfaceView
import android.view.SurfaceHolder
import android.view.MotionEvent
import androidx.appcompat.app.AppCompatActivity

class MainActivity : AppCompatActivity(), SurfaceHolder.Callback, Choreographer.FrameCallback {

    private lateinit var surfaceView: SurfaceView
    private var choreographer: Choreographer? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        surfaceView = findViewById(R.id.surfaceView)
        surfaceView.holder.addCallback(this)
        surfaceView.setOnTouchListener { _, event -> TouchForwarder.onTouch(event) }

        choreographer = Choreographer.getInstance()
        choreographer?.postFrameCallback(this)

        nativeInit()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        nativeSetSurface(holder.surface, surfaceView.width, surfaceView.height)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        // Vulkan 通过 swapchain 重建自动处理 surface 变化，无需重新创建 VkSurface
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {}

    override fun onDestroy() {
        super.onDestroy()
        nativeDestroy()
    }

    /** Choreographer vsync 回调：取出最新帧并渲染 */
    override fun doFrame(frameTimeNanos: Long) {
        nativeRenderFrame()
        choreographer?.postFrameCallback(this)
    }

    @dalvik.annotation.optimization.FastNative
    private external fun nativeInit()

    @dalvik.annotation.optimization.FastNative
    private external fun nativeSetSurface(surface: android.view.Surface, width: Int, height: Int)

    @dalvik.annotation.optimization.FastNative
    private external fun nativeRenderFrame()

    @dalvik.annotation.optimization.FastNative
    private external fun nativeDestroy()

    companion object {
        init {
            System.loadLibrary("land-native")
            System.loadLibrary("land-bridge")
        }
    }
}
