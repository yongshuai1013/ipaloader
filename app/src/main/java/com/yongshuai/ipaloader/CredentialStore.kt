package com.yongshuai.ipaloader

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Remembers the Apple ID + password between launches ("記住帳號").
 *
 * The email is kept in plain SharedPreferences. The password is
 * encrypted with an Android Keystore AES/GCM key that never leaves
 * the keystore; only the ciphertext and IV (Base64) are stored.
 * Neither value is ever written to the app log.
 */
object CredentialStore {

    private const val PREFS = "saved_credentials"
    private const val KEY_EMAIL = "email"
    private const val KEY_CIPHER = "password_cipher"
    private const val KEY_IV = "password_iv"
    private const val KEY_ALIAS = "ipaloader_cred_key"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128

    private fun getOrCreateKey(): SecretKey {
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (keyStore.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let {
            return it.secretKey
        }
        val generator =
            KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build()
        )
        return generator.generateKey()
    }

    /** Encrypts and stores the pair. Never throws; on any failure the saved pair is dropped. */
    fun save(context: Context, email: String, password: String) {
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, getOrCreateKey())
            val cipherText = cipher.doFinal(password.toByteArray(Charsets.UTF_8))
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
                .putString(KEY_EMAIL, email)
                .putString(KEY_CIPHER, Base64.encodeToString(cipherText, Base64.NO_WRAP))
                .putString(KEY_IV, Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
                .apply()
        } catch (e: Exception) {
            // Saving must never break a successful login; forget instead.
            clear(context)
        }
    }

    /**
     * Returns the saved (email, password), or null when nothing usable
     * is stored. Any failure (key invalidated, corrupt data) clears
     * the saved pair and returns null so the form starts blank.
     */
    fun load(context: Context): Pair<String, String>? {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val email = prefs.getString(KEY_EMAIL, null) ?: return null
        val cipherB64 = prefs.getString(KEY_CIPHER, null)
        val ivB64 = prefs.getString(KEY_IV, null)
        if (cipherB64 == null || ivB64 == null) {
            clear(context)
            return null
        }
        return try {
            val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
            val entry = keyStore.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry
                ?: throw IllegalStateException("keystore key missing")
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                entry.secretKey,
                GCMParameterSpec(GCM_TAG_BITS, Base64.decode(ivB64, Base64.NO_WRAP))
            )
            val plain = cipher.doFinal(Base64.decode(cipherB64, Base64.NO_WRAP))
            email to String(plain, Charsets.UTF_8)
        } catch (e: Exception) {
            clear(context)
            null
        }
    }

    fun clear(context: Context) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().clear().apply()
    }
}
