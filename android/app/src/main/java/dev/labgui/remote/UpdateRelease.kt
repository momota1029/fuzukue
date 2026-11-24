package dev.labgui.remote

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import java.io.InputStream
import java.io.OutputStream
import java.net.URI
import java.net.URISyntaxException
import java.security.DigestInputStream
import java.security.MessageDigest
import java.time.Instant

private val apkFileName = Regex("[A-Za-z0-9][A-Za-z0-9._+-]*\\.apk")
private val sha256Hex = Regex("[0-9a-fA-F]{64}")

@Serializable
internal data class UpdateRelease(
    val version: String,
    val versionCode: Int,
    @SerialName("package") val packageName: String,
    val sha256: String,
    val apk: String,
    @SerialName("minimum_protocol") val minimumProtocol: Int,
    @SerialName("release_notes_first_line") val releaseNotesFirstLine: String,
    @SerialName("built_at") val builtAt: String,
) {
    fun isNewerThan(installedVersionCode: Int): Boolean = versionCode > installedVersionCode

    // Use the configured feed's directory so a GitHub asset redirect doesn't set the APK URL.
    fun apkUrl(feed: HttpUrl): HttpUrl = feed.newBuilder().removePathSegment(feed.pathSize - 1)
        .addPathSegment(apk).query(null).fragment(null).build()

    companion object {
        fun parse(text: String, installedPackageName: String): UpdateRelease =
            Protocol.json.decodeFromString<UpdateRelease>(text).also {
                require(it.packageName == installedPackageName)
                require(it.version.isNotBlank() && it.version.lineSequence().count() == 1)
                require(it.versionCode > 0 && it.minimumProtocol > 0)
                require(sha256Hex.matches(it.sha256) && apkFileName.matches(it.apk))
                require(it.releaseNotesFirstLine.lineSequence().count() == 1)
                require(runCatching { Instant.parse(it.builtAt) }.isSuccess)
            }
    }
}

internal fun httpsUpdateUrl(value: String): HttpUrl? {
    val url = try {
        URI(value.trim())
    } catch (_: URISyntaxException) {
        return null
    }
    if (!url.scheme.equals("https", ignoreCase = true) || url.host == null ||
        url.rawUserInfo != null || url.rawFragment != null
    ) return null
    return url.toString().toHttpUrlOrNull()
}

internal fun copyAndVerifySha256(input: InputStream, output: OutputStream, expectedSha256: String): Boolean {
    val digest = MessageDigest.getInstance("SHA-256")
    DigestInputStream(input, digest).copyTo(output)
    val actualSha256 = digest.digest().joinToString("") { "%02x".format(it) }
    return actualSha256.equals(expectedSha256, ignoreCase = true)
}
