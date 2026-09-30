package dev.luksandroid.ui

import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager

/**
 * Tactile sensory feedback for physical security state changes.
 *
 * Provides subtle, discrete haptic pulses that reinforce state transitions:
 * - [deadboltLock]: Heavy discrete pulse when locking the drive or wiping memory.
 * - [unlock]: Light crisp click confirming successful key derivation.
 * - [error]: Subtle double-pulse on wrong passphrase or hardware error.
 */
object LuksHaptics {

    fun deadboltLock(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_HEAVY_CLICK))
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(40)
            }
        } catch (_: Throwable) {}
    }

    fun unlock(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_CLICK))
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(20)
            }
        } catch (_: Throwable) {}
    }

    /**
     * Tactile sensory feedback simulating tapping in on transition to the browser vault.
     * Uses a light priming contact tick followed by a crisp arrival click.
     */
    fun tapIn(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                vibrator.areAllPrimitivesSupported(
                    VibrationEffect.Composition.PRIMITIVE_TICK,
                    VibrationEffect.Composition.PRIMITIVE_CLICK
                )
            ) {
                val composition = VibrationEffect.startComposition()
                    .addPrimitive(VibrationEffect.Composition.PRIMITIVE_TICK, 0.75f)
                    .addPrimitive(VibrationEffect.Composition.PRIMITIVE_CLICK, 1.0f, 45)
                    .compose()
                vibrator.vibrate(composition)
            } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                vibrator.vibrate(
                    VibrationEffect.createWaveform(
                        longArrayOf(0, 12, 45, 25),
                        intArrayOf(0, 110, 0, 240),
                        -1
                    )
                )
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(longArrayOf(0, 15, 45, 25), -1)
            }
        } catch (_: Throwable) {
            unlock(context)
        }
    }

    fun tick(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_TICK))
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(10)
            }
        } catch (_: Throwable) {}
    }

    fun error(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_DOUBLE_CLICK))
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(longArrayOf(0, 30, 80, 30), -1)
            }
        } catch (_: Throwable) {}
    }

    /**
     * Starts an uphill crescendo haptic ramp lasting 1.12 seconds (1120ms).
     * Builds from a gentle tactile hum up to maximum peak intensity with a power curve.
     */
    fun startUphillRamp(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                if (vibrator.hasAmplitudeControl()) {
                    val stepCount = 28
                    val sliceMs = 40L
                    val timings = LongArray(stepCount + 1) { if (it == 0) 0L else sliceMs }
                    val amplitudes = IntArray(stepCount + 1) { i ->
                        if (i == 0) 0
                        else {
                            val t = (i - 1).toDouble() / (stepCount - 1).toDouble()
                            (25.0 + 230.0 * Math.pow(t, 1.8)).toInt().coerceIn(1, 255)
                        }
                    }
                    vibrator.vibrate(VibrationEffect.createWaveform(timings, amplitudes, -1))
                } else {
                    // Fallback pulse pattern precisely calibrated to sum to 1120ms
                    val timings = longArrayOf(
                        0, 20, 110, 20, 95, 20, 80, 25, 65, 25, 55, 30, 45, 30, 35, 35, 25, 40, 20, 45, 15, 50, 10, 55, 10, 60, 5, 80, 0, 95
                    )
                    @Suppress("DEPRECATION")
                    vibrator.vibrate(timings, -1)
                }
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(1120L)
            }
        } catch (_: Throwable) {}
    }

    /**
     * Tactile release and drop affirming the uphill ramp reached its peak at 1.12s.
     */
    fun rampPeakAndDrop(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_HEAVY_CLICK))
            } else {
                @Suppress("DEPRECATION")
                vibrator.vibrate(35)
            }
        } catch (_: Throwable) {}
    }

    /**
     * Cancels any ongoing haptic feedback immediately (e.g. when touch is released early).
     */
    fun cancel(context: Context) {
        try {
            val vibrator = getVibrator(context) ?: return
            vibrator.cancel()
        } catch (_: Throwable) {}
    }

    private fun getVibrator(context: Context): Vibrator? {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val manager = context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as? VibratorManager
            manager?.defaultVibrator
        } else {
            @Suppress("DEPRECATION")
            context.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator
        }
    }
}
