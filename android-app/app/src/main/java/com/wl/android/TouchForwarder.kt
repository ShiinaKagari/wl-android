package com.wl.android

import android.view.MotionEvent

class TouchForwarder(
    private val onTouch: (Int, Float, Float, Int, Int) -> Unit
) {
    var screenWidth: Int = 3392
    var screenHeight: Int = 2400

    companion object {
        const val TOUCH_DOWN = 0
        const val TOUCH_MOVE = 1
        const val TOUCH_UP = 2
        const val TOUCH_CANCEL = 3
        const val TOUCH_FRAME = 4
    }

    // FRAME-COALESCE: per-pointer MOVE cache flushed at the display frame
    // boundary (Choreographer vsync). A sliding finger produces 120Hz+ MOVE
    // streams; forwarding every one floods the server's touch injection and
    // KWin's input handling (fast swipes crashed KWin). Within one frame
    // time only the LAST MOVE per pointer is kept and committed when the
    // frame ticks — one MOVE per display frame, aligned to the render
    // rhythm, with full trajectory fidelity (final position per frame).
    //
    // DOWN/UP/CANCEL are NOT coalesced: they are state transitions and must
    // arrive immediately (a tap's DOWN-UP gap can be shorter than one frame;
    // caching would drop the DOWN and desync the touch state machine).
    private data class PendingMove(val x: Float, val y: Float, val timeMs: Int)
    private val pendingMoves = HashMap<Int, PendingMove>()
    private var framePosted = false
    private val choreographer by lazy { android.view.Choreographer.getInstance() }
    private val frameCallback = object : android.view.Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            framePosted = false
            if (pendingMoves.isEmpty()) {
                return
            }
            var latest = 0
            for ((id, mv) in pendingMoves) {
                onTouch(id, mv.x, mv.y, TOUCH_MOVE, mv.timeMs)
                if (mv.timeMs > latest) latest = mv.timeMs
            }
            pendingMoves.clear()
            // T-02: frame sentinel after the coalesced MOVE batch.
            onTouch(0, 0f, 0f, TOUCH_FRAME, latest)
        }
    }

    private fun postFrame() {
        if (!framePosted) {
            framePosted = true
            choreographer.postFrameCallback(frameCallback)
        }
    }

    fun handle(event: MotionEvent) {
        val actionMasked = event.actionMasked
        val actionIndex = event.actionIndex
        val pointerCount = event.pointerCount

        for (i in 0 until pointerCount) {
            val id = event.getPointerId(i)
            val x = event.getX(i)
            val y = event.getY(i)
            // Normalize to [0,1] using screen dimensions
            val nx = if (screenWidth > 0) (x / screenWidth.toFloat()).coerceIn(0f, 1f) else 0f
            val ny = if (screenHeight > 0) (y / screenHeight.toFloat()).coerceIn(0f, 1f) else 0f

            val phase = when {
                (actionMasked == MotionEvent.ACTION_DOWN || actionMasked == MotionEvent.ACTION_POINTER_DOWN)
                    && actionIndex == i -> TOUCH_DOWN
                (actionMasked == MotionEvent.ACTION_UP || actionMasked == MotionEvent.ACTION_POINTER_UP)
                    && actionIndex == i -> TOUCH_UP
                actionMasked == MotionEvent.ACTION_CANCEL -> TOUCH_CANCEL
                else -> TOUCH_MOVE
            }
            if (phase == TOUCH_MOVE) {
                // FRAME-COALESCE: keep the latest position; commit at the
                // display frame boundary.
                pendingMoves[id] = PendingMove(nx, ny, event.eventTime.toInt())
                postFrame()
            } else {
                // State transition: deliver immediately, drop any cached
                // MOVE for this pointer (the state has changed).
                //
                // FRAME-BOUNDARY FIX: no FRAME is sent right after DOWN/UP.
                // The wayland touch protocol groups events: DOWN belongs to
                // the same logical frame as the MOVE(s) that follow it, and
                // FRAME closes that group. Sending FRAME immediately after
                // DOWN split the group (DOWN→FRAME→MOVE) which KWin's touch
                // state machine rejected — plasmashell got a bogus stream and
                // was killed. The next MOVE batch (Choreographer callback)
                // or a subsequent UP terminates the frame instead.
                pendingMoves.remove(id)
                onTouch(id, nx, ny, phase, event.eventTime.toInt())
            }
        }
    }
}
