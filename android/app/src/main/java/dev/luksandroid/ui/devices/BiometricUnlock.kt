package dev.luksandroid.ui.devices

import android.content.Context
import android.content.ContextWrapper
import android.security.keystore.KeyPermanentlyInvalidatedException
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import dev.luksandroid.Trace
import dev.luksandroid.security.BiometricPassphraseVault
import dev.luksandroid.security.SecurePassphraseBuffer
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

internal tailrec fun Context.findFragmentActivity(): FragmentActivity? = when (this) {
    is FragmentActivity -> this
    is ContextWrapper -> baseContext.findFragmentActivity()
    else -> null
}

internal const val BIOMETRIC_NOTICE_RESET =
    "Fingerprint unlock was turned off because this phone's fingerprints changed. " +
        "Enter the passphrase; you can turn it back on below."
internal const val BIOMETRIC_NOTICE_UNAVAILABLE = "Fingerprint isn't available right now. Enter the passphrase."
internal const val BIOMETRIC_NOTICE_LOCKOUT = "Too many fingerprint attempts. Enter the passphrase."
internal const val BIOMETRIC_NOTICE_STALE =
    "The saved passphrase no longer unlocks this drive, so fingerprint unlock was turned off."

private sealed interface PromptResult {
    data class Success(val cipher: Cipher) : PromptResult
    data class Error(val code: Int) : PromptResult
}

/**
 * Bridges androidx's callback-based [BiometricPrompt] into a suspend call.
 *
 * The ON_STOP observer guarantees this always resolves if the activity is
 * backgrounded mid-prompt (app switch, screen lock) — without it, a
 * cancelled coroutine scope would leave the CompletableDeferred pending
 * forever and the caller hung.
 */
private suspend fun FragmentActivity.runBiometricPrompt(
    title: String,
    subtitle: String?,
    negativeText: String,
    cipher: Cipher,
): PromptResult {
    val result = CompletableDeferred<PromptResult>()
    val prompt = BiometricPrompt(
        this,
        ContextCompat.getMainExecutor(this),
        object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(authResult: BiometricPrompt.AuthenticationResult) {
                val c = authResult.cryptoObject?.cipher
                result.complete(if (c != null) PromptResult.Success(c) else PromptResult.Error(BiometricPrompt.ERROR_VENDOR))
            }

            override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                result.complete(PromptResult.Error(errorCode))
            }

            // onAuthenticationFailed (one rejected scan) is not terminal — the sensor keeps listening.
        },
    )
    val info = BiometricPrompt.PromptInfo.Builder()
        .setTitle(title)
        .apply { subtitle?.let { setSubtitle(it) } }
        .setNegativeButtonText(negativeText)
        .setAllowedAuthenticators(BIOMETRIC_STRONG)
        .build()

    val onStop = LifecycleEventObserver { _, event ->
        if (event == Lifecycle.Event.ON_STOP) {
            prompt.cancelAuthentication()
            result.complete(PromptResult.Error(BiometricPrompt.ERROR_CANCELED))
        }
    }
    lifecycle.addObserver(onStop)
    return try {
        prompt.authenticate(info, BiometricPrompt.CryptoObject(cipher))
        result.await()
    } finally {
        lifecycle.removeObserver(onStop)
        if (!result.isCompleted) prompt.cancelAuthentication()
    }
}

internal sealed interface BiometricUnlockOutcome {
    class Passphrase(val buffer: SecurePassphraseBuffer) : BiometricUnlockOutcome
    data class UseManual(val notice: String?) : BiometricUnlockOutcome
}

/** Fingerprint-gated equivalent of typing the passphrase for [partitionKey]. */
internal suspend fun obtainPassphraseWithBiometric(
    activity: FragmentActivity,
    context: Context,
    partitionKey: String,
    label: String,
): BiometricUnlockOutcome {
    if (!BiometricPassphraseVault.isAvailable(context)) {
        return BiometricUnlockOutcome.UseManual(null)
    }

    val cipher = try {
        withContext(Dispatchers.Default) {
            BiometricPassphraseVault.decryptCipherFor(context, partitionKey)
        }
    } catch (e: KeyPermanentlyInvalidatedException) {
        BiometricPassphraseVault.forget(context, partitionKey)
        return BiometricUnlockOutcome.UseManual(BIOMETRIC_NOTICE_RESET)
    } catch (e: Exception) {
        Trace.e("BiometricUnlock: decryptCipherFor failed: ${Trace.throwableSummary(e)}")
        return BiometricUnlockOutcome.UseManual(null)
    } ?: return BiometricUnlockOutcome.UseManual(null)

    return when (val result = activity.runBiometricPrompt("Unlock $label", null, "Use passphrase", cipher)) {
        is PromptResult.Error -> when (result.code) {
            BiometricPrompt.ERROR_LOCKOUT, BiometricPrompt.ERROR_LOCKOUT_PERMANENT ->
                BiometricUnlockOutcome.UseManual(BIOMETRIC_NOTICE_LOCKOUT)
            BiometricPrompt.ERROR_HW_UNAVAILABLE, BiometricPrompt.ERROR_HW_NOT_PRESENT, BiometricPrompt.ERROR_NO_BIOMETRICS ->
                BiometricUnlockOutcome.UseManual(BIOMETRIC_NOTICE_UNAVAILABLE)
            else -> BiometricUnlockOutcome.UseManual(null)
        }
        is PromptResult.Success -> try {
            BiometricUnlockOutcome.Passphrase(BiometricPassphraseVault.decrypt(context, partitionKey, result.cipher))
        } catch (e: AEADBadTagException) {
            BiometricPassphraseVault.forget(context, partitionKey)
            BiometricUnlockOutcome.UseManual(BIOMETRIC_NOTICE_RESET)
        } catch (e: Exception) {
            Trace.e("BiometricUnlock: decrypt failed: ${Trace.throwableSummary(e)}")
            BiometricUnlockOutcome.UseManual(null)
        }
    }
}

/**
 * Offers to save [passphrase] behind biometric auth for [partitionKey]. Does
 * NOT close [passphrase] — the caller owns that buffer's lifetime.
 */
internal suspend fun enrollBiometric(
    activity: FragmentActivity,
    context: Context,
    partitionKey: String,
    label: String,
    passphrase: SecurePassphraseBuffer,
): Boolean {
    if (!BiometricPassphraseVault.isAvailable(context) || BiometricPassphraseVault.isEnrolled(context, partitionKey)) {
        return false
    }

    val cipher = try {
        withContext(Dispatchers.Default) {
            // Drops any stale key left over from a previously skipped enrollment
            // so encryptCipherFor always starts from a fresh Keystore key.
            BiometricPassphraseVault.forget(context, partitionKey)
            BiometricPassphraseVault.encryptCipherFor(partitionKey)
        }
    } catch (e: Exception) {
        Trace.e("BiometricUnlock: encryptCipherFor failed: ${Trace.throwableSummary(e)}")
        return false
    }

    val result = activity.runBiometricPrompt("Enable fingerprint unlock", label, "Skip", cipher)
    val successCipher = (result as? PromptResult.Success)?.cipher
    if (successCipher == null) {
        BiometricPassphraseVault.forget(context, partitionKey)
        return false
    }
    return try {
        BiometricPassphraseVault.store(context, partitionKey, successCipher, passphrase)
        true
    } catch (e: Exception) {
        Trace.e("BiometricUnlock: store failed: ${Trace.throwableSummary(e)}")
        BiometricPassphraseVault.forget(context, partitionKey)
        false
    }
}
