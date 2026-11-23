package dev.ssmcp.mobile

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Only ciphertext and IV enter preferences. Backups are disabled in the manifest. */
class TokenStore(context: Context) {
    private val prefs = context.getSharedPreferences("pairing", Context.MODE_PRIVATE)
    private val alias = "ssmcp.pairing.v1"
    private fun key(create: Boolean): SecretKey? {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(alias, null) as? SecretKey)?.let { return it }
        if (!create) return null
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").run {
            init(KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setRandomizedEncryptionRequired(true).build())
            generateKey()
        }
    }
    fun save(origin: String, token: String) {
        require(token.isNotEmpty() && token.length <= 4096 && token.none { it <= ' ' || it == '\u007f' })
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key(true))
        // Binding ciphertext to its server prevents an edited preference from moving a token.
        cipher.updateAAD(origin.toByteArray(Charsets.UTF_8))
        val bytes = cipher.doFinal(token.toByteArray(Charsets.UTF_8))
        check(prefs.edit().putString("origin", origin).putString("iv", Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
            .putString("token", Base64.encodeToString(bytes, Base64.NO_WRAP)).commit()) { "Cannot save pairing" }
    }
    fun load(): Pair<String, String>? {
        val origin = prefs.getString("origin", null) ?: return null
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(false) ?: return null,
            GCMParameterSpec(128, Base64.decode(prefs.getString("iv", ""), Base64.NO_WRAP)))
        cipher.updateAAD(origin.toByteArray(Charsets.UTF_8))
        return origin to String(cipher.doFinal(Base64.decode(prefs.getString("token", ""), Base64.NO_WRAP)), Charsets.UTF_8)
    }
    fun delete() {
        check(prefs.edit().clear().commit()) { "Cannot delete pairing" }
        KeyStore.getInstance("AndroidKeyStore").apply { load(null); deleteEntry(alias) }
    }
}
