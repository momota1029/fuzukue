package dev.labgui.remote

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

internal class ConnectionSettings(context: Context) {
    private val prefs = context.getSharedPreferences("connection", Context.MODE_PRIVATE)
    val url: String get() = prefs.getString("url", "").orEmpty()
    var background: Boolean
        get() = prefs.getBoolean("background", false)
        set(value) { prefs.edit().putBoolean("background", value).apply() }
    var workerNotices: Boolean
        get() = prefs.getBoolean("worker_notices", false)
        set(value) { prefs.edit().putBoolean("worker_notices", value).apply() }
    var showPlanAboveConversation: Boolean
        get() = prefs.getBoolean("show_plan_above_conversation", false)
        set(value) { prefs.edit().putBoolean("show_plan_above_conversation", value).apply() }

    fun token(): String {
        val stored = prefs.getString("token", null) ?: return ""
        val (iv, ciphertext) = stored.split('.', limit = 2).map { Base64.decode(it, Base64.NO_WRAP) }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, iv))
        return cipher.doFinal(ciphertext).decodeToString()
    }

    fun save(url: String, token: String) {
        val editor = prefs.edit().putString("url", url)
        if (token.isEmpty()) {
            editor.remove("token")
        } else {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, key())
            val iv = Base64.encodeToString(cipher.iv, Base64.NO_WRAP)
            val ciphertext = Base64.encodeToString(cipher.doFinal(token.encodeToByteArray()), Base64.NO_WRAP)
            editor.putString("token", "$iv.$ciphertext")
        }
        editor.apply()
    }

    private fun key(): SecretKey {
        // Keep the existing Keystore alias so saved tokens survive a rename.
        val alias = "lab-gui-remote-token"
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (keyStore.getKey(alias, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").run {
            init(
                KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .build()
            )
            generateKey()
        }
    }
}
