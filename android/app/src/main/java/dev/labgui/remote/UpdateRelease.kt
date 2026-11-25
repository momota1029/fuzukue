package dev.labgui.remote

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.URI
import java.net.URISyntaxException
import java.security.DigestInputStream
import java.security.MessageDigest
import java.time.Instant
import javax.net.ssl.SSLException

internal fun checkUpdateSources(
    primary: UpdateSource,
    fallback: UpdateSource?,
    fetch: (UpdateSource) -> UpdateRelease,
): UpdateRelease {
    if (fallback == null) return fetch(primary)
    try {
        return fetch(primary)
    } catch (error: Exception) {
        val unavailable = when (error) {
            is SSLException -> false
            is UpdateProblem -> fallback.origin == UpdateOrigin.GitHub &&
                error.label in listOf(
                    R.string.update_check_failed, R.string.update_feed_missing, R.string.update_incompatible,
                )
            is IOException -> true
            is IllegalArgumentException -> fallback.origin == UpdateOrigin.GitHub
            else -> false
        }
        if (!unavailable) throw error
    }
    return fetch(fallback)
}

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
    @SerialName("maximum_protocol") val maximumProtocol: Int? = null,
) {
    fun isNewerThan(installedVersionCode: Int): Boolean = versionCode > installedVersionCode
    fun supports(server: UpdateProtocol): Boolean =
        minimumProtocol <= server.maximum && (maximumProtocol ?: minimumProtocol) >= server.minimum

    // Use the configured feed's directory so a GitHub asset redirect doesn't set the APK URL.
    fun apkUrl(feed: HttpUrl): HttpUrl = feed.newBuilder().removePathSegment(feed.pathSize - 1)
        .addPathSegment(apk).query(null).fragment(null).build()

    companion object {
        fun parse(text: String, installedPackageName: String): UpdateRelease =
            Protocol.json.decodeFromString<UpdateRelease>(text).also {
                require(it.packageName == installedPackageName)
                require(it.version.isNotBlank() && it.version.lineSequence().count() == 1)
                require(it.versionCode > 0 && it.minimumProtocol > 0)
                require(it.maximumProtocol == null || it.maximumProtocol >= it.minimumProtocol)
                require(sha256Hex.matches(it.sha256) && apkFileName.matches(it.apk))
                require(it.releaseNotesFirstLine.lineSequence().count() == 1)
                require(runCatching { Instant.parse(it.builtAt) }.isSuccess)
            }
    }
}

internal data class UpdateProtocol(val minimum: Int, val maximum: Int)

internal enum class UpdateChannel { Stable, Beta, Dev;
    val path get() = name.lowercase()
}

internal enum class UpdateOrigin { Server, Custom, GitHub }

internal enum class UpdateSourcePreference { Automatic, Server, Custom }

internal fun githubBetaSource() = UpdateSource(
    checkNotNull(httpsUpdateUrl("https://github.com/momota1029/fuzukue/releases/download/android-beta/android-latest.json")),
    UpdateOrigin.GitHub,
    UpdateChannel.Beta,
)

internal data class UpdateSource(val feed: HttpUrl, val origin: UpdateOrigin, val channel: UpdateChannel) {
    val key: String get() = "$origin:${channel.path}:$feed"
}

internal fun serverUpdateSource(connection: String, channel: UpdateChannel): UpdateSource? {
    val endpoint = socketUrl(connection)?.let(::httpsUpdateUrl) ?: return null
    val feed = endpoint.newBuilder().removePathSegment(endpoint.pathSize - 1)
        .addPathSegment("android").addPathSegment(channel.path).addPathSegment("android-latest.json").build()
    return UpdateSource(feed, UpdateOrigin.Server, channel)
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
