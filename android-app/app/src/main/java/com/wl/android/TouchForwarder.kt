package com.wl.android

import android.view.MotionEvent

class TouchForwarder(private val onTouch: (Int, Float, Float, Int, Int) -> Unit) {

    companion object {
        const val TOUCH_DOWN = 0
        const val TOUCH_MOVE = 1
        const val TOUCH_UP = 2
        const val TOUCH_CANCEL = 3
        const val TOUCH_FRAME = 4
    }

    fun handle(event: MotionEvent) {
        val actionMasked = event.actionMasked
        val actionIndex = event.actionIndex
        val pointerCount = event.pointerCount

        for (i in 0 until pointerCount) {
            val id = event.getPointerId(i)
            val x = if (event.xPrecision > 0) event.getX(i) / event.xPrecision else event.getX(i)
            val y = if (event.yPrecision > 0) event.getY(i) / event.yPrecision else event.getY(i)
            // Normalize to [0,1] using device resolution
            val nx = x / event.device.width.toFloat()
            val ny = y / event.device.height.toFloat()

            val phase = when {
                (actionMasked == MotionEvent.ACTION_DOWN || actionMasked == MotionEvent.ACTION_POINTER_DOWN)
                    && actionIndex == i -> TOUCH_DOWN
                (actionMasked == MotionEvent.ACTION_UP || actionMasked == MotionEvent.ACTION_POINTER_UP)
                    && actionIndex == i -> TOUCH_UP
                actionMasked == MotionEvent.ACTION_CANCEL -> TOUCH_CANCEL
                else -> TOUCH_MOVE
            }
            onTouch(id, nx.coerceIn(0f, 1f), ny.coerceIn(0f, 1f), phase, event.eventTime.toInt())
        }
        // T-02: frame sentinel after all pointers
        onTouch(0, 0f, 0f, TOUCH_FRAME, event.eventTime.toInt())
    }
}
