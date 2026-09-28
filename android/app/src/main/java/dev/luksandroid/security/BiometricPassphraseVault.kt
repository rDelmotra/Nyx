package dev.luksandroid.security

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import java.nio.ByteBuffer
import java.security.KeyStore
import java.util.Arrays
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Wraps a LUKS passphrase in a hardware-backed Keystore key gated by
 * BiometricPrompt, so unlocking with a fingerprint still runs the same KDF
 * as typing the passphrase in — this only replaces typing, it does not
 * cache the derived volume key.
 *
 * One key + one wrapped blob per [partitionKey] (see LuksDevice.biometricKeyFor
 * in LuksHandles.kt). The key requires a fresh biometric auth for every
 * decrypt (auth-per-use, not a time window) and is invalidated if the
 * device's fingerprint enrollment changes.
 */
object BiometricPassphraseVault {

    private const val KEYSTORE = "AndroidKeyStore"
    private const val PREFS = "biometric_vault"
    private const val TRANSFORM = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128

    private fun alias(partitionKey: String) = "luks_biometric_$partitionKey"
    private fun ivPrefKey(partitionKey: String) = "iv_$partitionKey"
    private fun blobPrefKey(partitionKey: String) = "blob_$partitionKey"

    private fun prefs(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun isAvailable(context: Context): Boolean =
        BiometricManager.from(context).canAuthenticate(BIOMETRIC_STRONG) ==
            BiometricManager.BIOMETRIC_SUCCESS

    fun isEnrolled(context: Context, partitionKey: String): Boolean =
        prefs(context).contains(blobPrefKey(partitionKey))

    /** Drops the stored blob and the Keystore key. Call after KeyPermanentlyInvalidatedException. */
    fun forget(context: Context, partitionKey: String) {
        prefs(context).edit()
            .remove(ivPrefKey(partitionKey))
            .remove(blobPrefKey(partitionKey))
            .apply()
        runCatching {
            KeyStore.getInstance(KEYSTORE).apply { load(null) }.deleteEntry(alias(partitionKey))
        }
    }

    /** Cipher for a BiometricPrompt.CryptoObject; on success pass it to [store]. */
    fun encryptCipherFor(partitionKey: String): Cipher {
        val key = getOrCreateKey(partitionKey)
        return Cipher.getInstance(TRANSFORM).apply { init(Cipher.ENCRYPT_MODE, key) }
    }

    /** Encrypts [passphrase] with an already biometric-authorized [cipher] and persists the blob. */
    fun store(context: Context, partitionKey: String, cipher: Cipher, passphrase: SecurePassphraseBuffer) {
        val ciphertext = passphrase.withBuffer { buf, len ->
            val bytes = ByteArray(len)
            val dup = buf.duplicate().apply { position(0) }
            dup.get(bytes, 0, len)
            try {
                cipher.doFinal(bytes)
            } finally {
                Arrays.fill(bytes, 0)
            }
        }
        prefs(context).edit()
            .putString(ivPrefKey(partitionKey), Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
            .putString(blobPrefKey(partitionKey), Base64.encodeToString(ciphertext, Base64.NO_WRAP))
            .apply()
    }

    /**
     * Cipher for a BiometricPrompt.CryptoObject; on success pass it to [decrypt].
     * Returns null if nothing is enrolled for this partition. Throws
     * [KeyPermanentlyInvalidatedException] if biometric enrollment changed since
     * [store] — callers must catch it, call [forget], and fall back to manual entry.
     */
    fun decryptCipherFor(context: Context, partitionKey: String): Cipher? {
        val ivB64 = prefs(context).getString(ivPrefKey(partitionKey), null) ?: return null
        val key = getOrCreateKey(partitionKey)
        val iv = Base64.decode(ivB64, Base64.NO_WRAP)
        return Cipher.getInstance(TRANSFORM).apply {
            init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, iv))
        }
    }

    /** Decrypts with an already biometric-authorized [cipher] from [decryptCipherFor]. */
    fun decrypt(context: Context, partitionKey: String, cipher: Cipher): SecurePassphraseBuffer {
        val blobB64 = requireNotNull(prefs(context).getString(blobPrefKey(partitionKey), null)) {
            "no stored passphrase for $partitionKey"
        }
        val ciphertext = Base64.decode(blobB64, Base64.NO_WRAP)
        val plain = cipher.doFinal(ciphertext)
        try {
            val direct = ByteBuffer.allocateDirect(plain.size)
            direct.put(plain)
            return SecurePassphraseBuffer.of(direct, plain.size)
        } finally {
            Arrays.fill(plain, 0)
        }
    }

    private fun getOrCreateKey(partitionKey: String): SecretKey {
        val ks = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (ks.getKey(alias(partitionKey), null) as? SecretKey)?.let { return it }

        val builder = KeyGenParameterSpec.Builder(
            alias(partitionKey),
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setUserAuthenticationRequired(true)
            .setInvalidatedByBiometricEnrollment(true)
            // Leaving the validity duration/type unset (rather than calling
            // setUserAuthenticationParameters explicitly) already defaults to
            // "every use" + AUTH_BIOMETRIC_STRONG per AOSP's own
            // KeyGenParameterSpec.Builder field defaults — confirmed against
            // frameworks/base source, not assumed. Same as Aegis's key setup.

        val keyGen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        keyGen.init(builder.build())
        return keyGen.generateKey()
    }
}
