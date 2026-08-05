package com.wl.android

import android.app.Activity
import android.app.KeyguardManager
import android.content.Context
import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.WindowManager
import android.widget.TextView

class MainActivity : Activity(), SurfaceHolder.Callback, StatusListener {
    private lateinit var surfaceView: SurfaceView
    private lateinit var statusText: TextView
    private lateinit var collector: ScreenInfoCollector
    private lateinit var touchForwarder: TouchForwarder
    private var nativeHandle: Long = 0
    private val socketPath = "/data/local/tmp/wl-android/land.sock"

    // CONN-STATE: event-driven — native calls onStateChanged on connection
    // state changes (no polling). Runs on the native recv thread; hop to the
    // main thread to touch the view hierarchy.
    override fun onStateChanged(state: Int) {
        runOnUiThread {
            when (state) {
                2 -> statusText.visibility = android.view.View.GONE // Active: normal display
                4 -> { statusText.text = "Disconnected"; statusText.visibility = android.view.View.VISIBLE }
                else -> { statusText.text = "Reconnection"; statusText.visibility = android.view.View.VISIBLE }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.O_MR1) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
        }
        @Suppress("DEPRECATION")
        window.addFlags(WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED)
        @Suppress("DEPRECATION")
        window.addFlags(WindowManager.LayoutParams.FLAG_DISMISS_KEYGUARD)

        surfaceView = SurfaceView(this).apply {
            holder.addCallback(this@MainActivity)
            // SurfaceView's Surface lives ABOVE normal views in the window
            // Z-order by default — the status overlay would be hidden behind
            // it. Media-overlay Z keeps the Surface below regular views so
            // the Disconnected/Reconnection TextView stays visible on top.
            setZOrderMediaOverlay(true)
        }

        // CONN-STATE overlay: a semi-transparent black layer with the
        // Disconnected / Reconnection status text, shown above the SurfaceView
        // whenever nativeGetState is not Active. The native side blanks the
        // actual surface; this layer only carries the text.
        statusText = TextView(this).apply {
            setTextColor(android.graphics.Color.WHITE)
            textSize = 24f
            gravity = android.view.Gravity.CENTER
            background = android.graphics.drawable.ColorDrawable(android.graphics.Color.argb(180, 0, 0, 0))
            visibility = android.view.View.GONE
        }
        setContentView(
            android.widget.FrameLayout(this).apply {
                addView(surfaceView, android.widget.FrameLayout.LayoutParams(
                    android.widget.FrameLayout.LayoutParams.MATCH_PARENT,
                    android.widget.FrameLayout.LayoutParams.MATCH_PARENT
                ))
                addView(statusText, android.widget.FrameLayout.LayoutParams(
                    android.widget.FrameLayout.LayoutParams.MATCH_PARENT,
                    android.widget.FrameLayout.LayoutParams.MATCH_PARENT
                ))
            }
        )

        // SurfaceView must be focusable to receive hardware key events.
        // isFocusableInTouchMode lets the surface grab focus on first tap,
        // which is how it gets KEYCODE_* input once the user interacts.
        surfaceView.isFocusable = true
        surfaceView.isFocusableInTouchMode = true
        // requestFocus only works after the view is attached to a window,
        // so post() it to run after the first layout pass.
        surfaceView.post { surfaceView.requestFocus() }

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
        // CONN-STATE: register the event-driven status listener (native calls
        // onStateChanged on connection changes — no polling).
        if (nativeHandle != 0L) {
            NativeBridge.nativeSetStatusListener(nativeHandle, this)
        }
    }

    override fun onResume() {
        super.onResume()

        val km = getSystemService(Context.KEYGUARD_SERVICE) as? KeyguardManager
        if (km?.isKeyguardLocked == true) {
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.O) {
                km.requestDismissKeyguard(this, null)
            }
        }

        collector.start()

        // Focus can be stolen (e.g. keyguard dismissal, dialog), re-grab after resume.
        surfaceView.post { surfaceView.requestFocus() }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        // DESIGN DECISION: forward-then-consume.
        // dispatchKeyEvent runs BEFORE the focused view; returning true here consumes
        // the event so the SurfaceView never sees it -> no double-handling.
        // BACK and HOME are system-owned: never forward, let super handle them so the
        // user can still exit the app / reach the launcher.
        if (event.keyCode == KeyEvent.KEYCODE_BACK || event.keyCode == KeyEvent.KEYCODE_HOME) {
            return super.dispatchKeyEvent(event)
        }
        // Forward any other key. scanCode 0 (virtual/soft keys) is still sent: the
        // server's xkbcommon map drops it, physical keyboards are the target.
        val state = if (event.action == KeyEvent.ACTION_DOWN) 1 else 0
        if (nativeHandle != 0L) {
            NativeBridge.nativeOnKey(nativeHandle, event.scanCode, state, event.eventTime.toInt())
        }
        return true
    }

    override fun onPause() {
        collector.stop()
        super.onPause()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (nativeHandle != 0L) {
            NativeBridge.nativeSetSurface(nativeHandle, holder.surface)
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        // SURFACE-REARM: on process restart (e.g. install -r) Android may
        // re-deliver an existing surface via surfaceChanged without a fresh
        // surfaceCreated — the SurfaceView was created before nativeInit
        // finished, or the surface survived the restart. Re-assert the
        // window so the CPU render path is armed even then (idempotent).
        if (nativeHandle != 0L) {
            NativeBridge.nativeSetSurface(nativeHandle, holder.surface)
        }
        collector.emit()
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        if (nativeHandle != 0L) {
            NativeBridge.nativeSetSurface(nativeHandle, null)
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        NativeBridge.nativeDestroy(nativeHandle)
        nativeHandle = 0
    }
}
