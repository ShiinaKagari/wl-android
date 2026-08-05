package com.wl.android

import android.app.Activity
import android.app.KeyguardManager
import android.content.Context
import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.KeyEvent
import android.view.WindowManager
import android.widget.TextView

class MainActivity : Activity(), SurfaceHolder.Callback, StatusListener {
    private lateinit var surfaceView: SurfaceView
    private lateinit var statusText: TextView
    private lateinit var collector: ScreenInfoCollector
    private lateinit var touchForwarder: TouchForwarder
    private lateinit var settingsPanel: android.widget.LinearLayout
    private lateinit var scaleLabel: TextView
    private lateinit var modeLabel: TextView
    private var nativeHandle: Long = 0
    private val socketPath = "/data/local/tmp/wl-android/land.sock"
    // Render scale: only ≤1x is safe — >1x makes KWin render ABOVE the
    // physical panel resolution, which crashed it (black screen). ≤1x
    // renders at a lower resolution and SurfaceFlinger stretches the
    // smaller buffer to fill the fullscreen SurfaceView.
    private val scaleOptions = floatArrayOf(0.5f, 0.75f, 1f)
    private val scaleLabels = arrayOf("0.5x", "0.75x", "1x")
    private var scaleIndex = 2 // default 1x
    private val modeNames = arrayOf("Free", "Vsync-align", "Performance", "Power-save")

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
                addView(buildSettingsPanel(), android.widget.FrameLayout.LayoutParams(
                    android.widget.FrameLayout.LayoutParams.WRAP_CONTENT,
                    android.widget.FrameLayout.LayoutParams.WRAP_CONTENT
                ).apply { gravity = android.view.Gravity.RIGHT or android.view.Gravity.CENTER_VERTICAL })
                addView(buildSettingsHandle(), android.widget.FrameLayout.LayoutParams(
                    dp(48), dp(48)
                ).apply { gravity = android.view.Gravity.RIGHT or android.view.Gravity.CENTER_VERTICAL })
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

        collector = ScreenInfoCollector(this) { w, h, ref, dpi, mode ->
            // Touch normalization must use the PHYSICAL SurfaceView size
            // (event.getX/Y are physical coords), NOT the render target
            // size w/h — the server maps the normalized [0,1] coords onto
            // its own render resolution. Using w/h here shifts every touch
            // by the scale factor.
            val m = resources.displayMetrics
            touchForwarder.screenWidth = m.widthPixels
            touchForwarder.screenHeight = m.heightPixels
            if (nativeHandle != 0L) {
                NativeBridge.nativeOnConfig(nativeHandle, w, h, ref, dpi, mode)
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

    /** Sidebar settings menu: a translucent panel on the right edge with the
     * render-scale selector and the frame-pacing mode selector. Each selector
     * is a label + cycling button; tapping the panel toggles it. The panel
     * does not steal touches from the SurfaceView when collapsed. */
    private fun buildSettingsPanel(): android.view.View {
        scaleLabel = TextView(this).apply {
            setTextColor(android.graphics.Color.WHITE)
            textSize = 16f
            text = "Scale: 1x"
        }
        val scaleBtn = android.widget.Button(this).apply {
            text = "0.5x / 0.75x / 1x"
            setOnClickListener {
                scaleIndex = (scaleIndex + 1) % scaleOptions.size
                applyScale()
            }
        }
        modeLabel = TextView(this).apply {
            setTextColor(android.graphics.Color.WHITE)
            textSize = 16f
            text = "Pacing: Free"
        }
        val modeBtn = android.widget.Button(this).apply {
            text = "Free / Vsync / Perf / Power"
            setOnClickListener {
                collector.frameMode = (collector.frameMode + 1) % 4
                modeLabel.text = "Pacing: ${modeNames[collector.frameMode]}"
                collector.emit()
            }
        }
        settingsPanel = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
            setPadding(dp(12), dp(12), dp(12), dp(12))
            background = android.graphics.drawable.ColorDrawable(android.graphics.Color.argb(190, 20, 20, 20))
            addView(scaleLabel)
            addView(scaleBtn, android.widget.LinearLayout.LayoutParams(
                android.widget.LinearLayout.LayoutParams.MATCH_PARENT,
                android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
            ).apply { topMargin = dp(4); bottomMargin = dp(8) })
            addView(modeLabel)
            addView(modeBtn, android.widget.LinearLayout.LayoutParams(
                android.widget.LinearLayout.LayoutParams.MATCH_PARENT,
                android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
            ).apply { topMargin = dp(4) })
        }
        // A close button collapses the panel. The buttons inside get their
        // clicks directly (Android event dispatch: the deepest view consumes
        // first; a panel-wide click listener would only fire on empty area).
        val closeBtn = android.widget.Button(this).apply {
            text = "✕ Close"
            setOnClickListener {
                settingsPanel.visibility = android.view.View.GONE
            }
        }
        settingsPanel.addView(closeBtn, android.widget.LinearLayout.LayoutParams(
            android.widget.LinearLayout.LayoutParams.MATCH_PARENT,
            android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(8) })
        settingsPanel.visibility = android.view.View.GONE
        return settingsPanel
    }

    /** Small handle on the right edge that toggles the settings panel open. */
    private fun buildSettingsHandle(): android.view.View {
        return android.widget.TextView(this).apply {
            text = "⚙"
            textSize = 24f
            gravity = android.view.Gravity.CENTER
            setTextColor(android.graphics.Color.WHITE)
            background = android.graphics.drawable.ColorDrawable(android.graphics.Color.argb(150, 20, 20, 20))
            setOnClickListener {
                settingsPanel.visibility = android.view.View.VISIBLE
            }
        }
    }

    private fun applyScale() {
        collector.scale = scaleOptions[scaleIndex]
        scaleLabel.text = "Scale: ${scaleLabels[scaleIndex]}"
        // Set the App-side buffer geometry to the render target too, so
        // ANativeWindow_lock hands back the scaled size; SurfaceFlinger
        // stretches it to the fullscreen SurfaceView. Use displayMetrics
        // (rotated to the App orientation) like the collector.
        if (nativeHandle != 0L) {
            val m = resources.displayMetrics
            val rw = (m.widthPixels * collector.scale).toInt().coerceAtLeast(1)
            val rh = (m.heightPixels * collector.scale).toInt().coerceAtLeast(1)
            NativeBridge.nativeSetRenderSize(nativeHandle, rw, rh)
        }
        collector.emit()
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

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
