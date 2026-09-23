package dev.ranch.android

import android.os.Build
import android.view.View
import android.view.WindowInsets

/**
 * targetSdk 35 enforces edge-to-edge on Android 15+, where
 * `windowSoftInputMode=adjustResize` is ignored — the window no longer
 * resizes when the IME opens, so bottom-anchored UI hides behind the
 * keyboard. Pad the content view from the IME + system-bar + cutout
 * insets ourselves. (Pre-Android-15 devices still resize via
 * adjustResize, so the listener is API-35+ only to avoid double padding.)
 */
fun applyEdgeToEdgeInsets(contentView: View) {
    if (Build.VERSION.SDK_INT < 35) return
    contentView.setOnApplyWindowInsetsListener { v, insets ->
        fun t(type: Int) = insets.getInsets(type)
        val ime = t(WindowInsets.Type.ime())
        val bars = t(WindowInsets.Type.systemBars())
        val cut = t(WindowInsets.Type.displayCutout())
        v.setPadding(
            maxOf(bars.left, cut.left),
            maxOf(bars.top, cut.top),
            maxOf(bars.right, cut.right),
            maxOf(ime.bottom, bars.bottom, cut.bottom),
        )
        insets
    }
}
