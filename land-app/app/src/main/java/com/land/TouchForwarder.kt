package com.land

import android.view.MotionEvent

/** 触摸事件捕获，暂未接入转发通道 */
object TouchForwarder {
    fun onTouch(event: MotionEvent): Boolean = true
}
