package com.land

import android.view.MotionEvent

/**
 * 触摸事件引擎。
 * 支持：单指拖动、双指缩放、双指滚动、长按。
 * 将手势翻译为 Wayland 触摸协议通过 JNI 发送。
 */
object TouchForwarder {

    // 手势状态
    private var gestureState = GestureState.IDLE
    private enum class GestureState { IDLE, TOUCH, SCROLL, PINCH, LONG_PRESS }

    // 触控点历史
    private val activePointers = HashMap<Int, PointerInfo>()
    private var firstPointerId: Int = -1
    private var longPressRunnable: Runnable? = null

    // 双指状态
    private var pinchSpan0: Float = 0f
    private var scrollAccumX = 0f
    private var scrollAccumY = 0f

    private data class PointerInfo(
        val id: Int,
        var x: Float,
        var y: Float,
        var downTime: Long,
    )

    /** 处理 MotionEvent，返回 true 表示消费 */
    fun onTouch(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN,
            MotionEvent.ACTION_POINTER_DOWN -> onPointerDown(event)
            MotionEvent.ACTION_MOVE -> onPointerMove(event)
            MotionEvent.ACTION_UP,
            MotionEvent.ACTION_POINTER_UP -> onPointerUp(event)
            MotionEvent.ACTION_CANCEL -> onCancel()
        }
        return true
    }

    private fun onPointerDown(event: MotionEvent) {
        val idx = event.actionIndex
        val id = event.getPointerId(idx)
        val x = event.getX(idx)
        val y = event.getY(idx)

        activePointers[id] = PointerInfo(id, x, y, System.currentTimeMillis())

        when (activePointers.size) {
            1 -> {
                firstPointerId = id
                gestureState = GestureState.TOUCH
                nativeTouchDown(id, x, y)
            }
            2 -> {
                gestureState = GestureState.PINCH
                pinchSpan0 = calculateSpan(event)
                nativeTouchUp(firstPointerId, event.getX(event.findPointerIndex(firstPointerId)),
                    event.getY(event.findPointerIndex(firstPointerId)))
            }
        }
    }

    private fun onPointerMove(event: MotionEvent) {
        when (gestureState) {
            GestureState.IDLE -> {}
            GestureState.LONG_PRESS -> {}
            GestureState.TOUCH -> {
                val idx = event.findPointerIndex(firstPointerId)
                if (idx >= 0) {
                    val x = event.getX(idx)
                    val y = event.getY(idx)
                    nativeTouchMove(firstPointerId, x, y)
                    activePointers[firstPointerId]?.let {
                        it.x = x; it.y = y
                    }
                }
            }

            GestureState.PINCH -> {
                if (event.pointerCount >= 2) {
                    // 双指：区分缩放和滚动
                    val span = calculateSpan(event)
                    val scaleFactor = if (pinchSpan0 > 0) span / pinchSpan0 else 1f

                    // 累积滚动偏移
                    var dx = 0f; var dy = 0f
                    for (i in 0 until event.pointerCount) {
                        val id = event.getPointerId(i)
                        val old = activePointers[id]
                        if (old != null) {
                            dx += event.getX(i) - old.x
                            dy += event.getY(i) - old.y
                        }
                    }
                    scrollAccumX += dx
                    scrollAccumY += dy

                    // 判断意图
                    val isScale = kotlin.math.abs(scaleFactor - 1f) > 0.05f
                    val isScroll = kotlin.math.abs(scrollAccumX) > 20f ||
                            kotlin.math.abs(scrollAccumY) > 20f

                    if (isScale && !isScroll) {
                        nativePinch(scaleFactor)
                        pinchSpan0 = span
                    } else if (isScroll) {
                        gestureState = GestureState.SCROLL
                        nativeScroll(scrollAccumX, scrollAccumY)
                        scrollAccumX = 0f; scrollAccumY = 0f
                    }

                    // 更新历史
                    for (i in 0 until event.pointerCount) {
                        val id = event.getPointerId(i)
                        activePointers[id]?.let { it.x = event.getX(i); it.y = event.getY(i) }
                    }
                }
            }

            GestureState.SCROLL -> {
                var dx = 0f; var dy = 0f
                for (i in 0 until event.pointerCount) {
                    val id = event.getPointerId(i)
                    val old = activePointers[id]
                    if (old != null) {
                        dx += event.getX(i) - old.x
                        dy += event.getY(i) - old.y
                        activePointers[id]?.let { it.x = event.getX(i); it.y = event.getY(i) }
                    }
                }
                nativeScroll(dx, dy)
            }
            else -> {}
        }
    }

    private fun onPointerUp(event: MotionEvent) {
        val idx = event.actionIndex
        val id = event.getPointerId(idx)
        val x = event.getX(idx)
        val y = event.getY(idx)

        when {
            gestureState == GestureState.TOUCH && activePointers.size == 1 -> {
                nativeTouchUp(id, x, y)
                gestureState = GestureState.IDLE
            }
            gestureState == GestureState.SCROLL && activePointers.size <= 2 -> {
                nativeScrollEnd()
                gestureState = GestureState.IDLE
            }
            gestureState == GestureState.PINCH && activePointers.size <= 2 -> {
                nativePinchEnd()
                gestureState = GestureState.IDLE
            }
            else -> {
                if (activePointers.size <= 1) {
                    nativeTouchUp(id, x, y)
                    gestureState = GestureState.IDLE
                }
            }
        }

        activePointers.remove(id)
        if (activePointers.isEmpty()) {
            firstPointerId = -1
        }
    }

    private fun onCancel() {
        for ((id, _) in activePointers) {
            nativeTouchUp(id, 0f, 0f)
        }
        activePointers.clear()
        firstPointerId = -1
        gestureState = GestureState.IDLE
        scrollAccumX = 0f; scrollAccumY = 0f
    }

    private fun calculateSpan(event: MotionEvent): Float {
        if (event.pointerCount < 2) return 0f
        val dx = event.getX(0) - event.getX(1)
        val dy = event.getY(0) - event.getY(1)
        return kotlin.math.sqrt(dx * dx + dy * dy)
    }

    // JNI — 使用 @CriticalNative (无 JNIEnv/JClass 参数)
    @dalvik.annotation.optimization.CriticalNative
    private external fun nativeTouchDown(id: Int, x: Float, y: Float)

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativeTouchMove(id: Int, x: Float, y: Float)

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativeTouchUp(id: Int, x: Float, y: Float)

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativePinch(scale: Float)

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativePinchEnd()

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativeScroll(dx: Float, dy: Float)

    @dalvik.annotation.optimization.CriticalNative
    private external fun nativeScrollEnd()
}
