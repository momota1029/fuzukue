package dev.labgui.remote

import android.content.Context
import android.content.pm.PackageManager
import android.content.pm.Signature
import okhttp3.HttpUrl
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import java.io.File
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.TimeUnit

private const val MAX_MANIFEST_BYTES = 32_768L
private const val MAX_APK_BYTES = 256L * 1024 * 1024
private object DiscardApkBytes : OutputStream() {
    override fun write(value: Int) {}
    override fun write(bytes: ByteArray, offset: Int, length: Int) {}
}

internal class UpdateProblem(val label: Int) : IOException()

internal class UpdateDownload(
    private val context: Context,
    client: OkHttpClient = OkHttpClient(),
) {
    private val directory = File(context.filesDir, "android-updates")
    private val partial = File(directory, "download.part")
    private val resume = File(directory, "download.resume")
    private val updateClient = client.newBuilder()
        .followRedirects(false)
        .callTimeout(5, TimeUnit.MINUTES)
        .build()

    fun check(source: UpdateSource, token: String, protocol: UpdateProtocol?): UpdateRelease =
        get(source, source.feed, token).use { response ->
            requireSuccess(response, R.string.update_check_failed)
            val body = response.body.source()
            require(!body.request(MAX_MANIFEST_BYTES + 1))
            val release = UpdateRelease.parse(body.readUtf8(), context.packageName)
            val server = if (source.origin == UpdateOrigin.Server) {
                val maximum = response.header("X-Fuzukue-Protocol")?.toIntOrNull()
                require(maximum != null && maximum > 0 && release.maximumProtocol != null)
                UpdateProtocol(1, maximum)
            } else protocol
            if (server != null && !release.supports(server)) throw UpdateProblem(R.string.update_incompatible)
            release
        }

    fun verifiedFile(release: UpdateRelease): File = File(directory, "${release.sha256.lowercase()}.apk")

    fun download(
        source: UpdateSource,
        release: UpdateRelease,
        token: String,
        progress: (Long, Long?) -> Unit,
    ): File {
        if (!directory.isDirectory && !directory.mkdirs()) throw IOException("Cannot create update directory")
        val verified = verifiedFile(release)
        if (verified.isFile && verify(verified, release)) return verified
        verified.delete()
        val url = release.apkUrl(source.feed)
        val identity = "$url\n${release.sha256.lowercase()}"
        val validator = if (resume.isFile && partial.isFile) {
            val lines = resume.readLines()
            lines.getOrNull(2)?.takeIf { lines.size == 3 && lines.take(2).joinToString("\n") == identity }
                ?.let(::strongEtag)
        } else null
        val offset = partial.length().takeIf { validator != null && it in 1 until MAX_APK_BYTES } ?: 0L
        if (!transfer(source, url, token, identity, offset, validator, progress)) {
            clearPartial()
            if (!transfer(source, url, token, identity, 0, null, progress)) {
                throw UpdateProblem(R.string.update_download_failed)
            }
        }
        if (!verify(partial, release)) {
            clearPartial()
            throw UpdateProblem(R.string.update_apk_invalid)
        }
        if (!partial.renameTo(verified)) throw IOException("Cannot store verified APK")
        resume.delete()
        verified.setLastModified(System.currentTimeMillis())
        // Retain the newest verified APK and one predecessor.
        directory.listFiles { file -> file.extension == "apk" }?.filter { it != verified }
            ?.sortedByDescending(File::lastModified)?.drop(1)?.forEach { it.delete() }
        return verified
    }

    @Suppress("DEPRECATION") // The integer flags overload also works on API 29–32.
    fun verify(file: File, release: UpdateRelease): Boolean {
        if (file.length() !in 1..MAX_APK_BYTES ||
            !file.inputStream().use { copyAndVerifySha256(it, DiscardApkBytes, release.sha256) }
        ) return false
        val pm = context.packageManager
        // GET_SIGNING_CERTIFICATES makes the platform verify the archive signature while parsing.
        val archive = pm.getPackageArchiveInfo(file.absolutePath, PackageManager.GET_SIGNING_CERTIFICATES) ?: return false
        val installed = pm.getPackageInfo(context.packageName, PackageManager.GET_SIGNING_CERTIFICATES)
        return archive.packageName == release.packageName && release.packageName == installed.packageName &&
            archive.longVersionCode == release.versionCode.toLong() && archive.longVersionCode > installed.longVersionCode &&
            sameApkSigners(archive.signingInfo?.apkContentsSigners, installed.signingInfo?.apkContentsSigners)
    }

    private fun clearPartial() {
        resume.delete()
        partial.delete()
    }

    private fun transfer(
        source: UpdateSource,
        url: HttpUrl,
        token: String,
        identity: String,
        offset: Long,
        validator: String?,
        progress: (Long, Long?) -> Unit,
    ): Boolean = get(source, url, token, offset, validator).use { response ->
        if (response.code == 416 && offset > 0) return false
        requireSuccess(response, R.string.update_download_failed)
        val body = response.body
        val etag = strongEtag(response.header("ETag"))
        val append = response.code == 206
        val total = if (append) {
            if (offset == 0L || etag != validator) return false
            resumedLength(response.header("Content-Range"), offset, body.contentLength()) ?: return false
        } else {
            if (response.code != 200) return false
            clearPartial()
            require(body.contentLength() <= MAX_APK_BYTES)
            if (etag != null) resume.writeText("$identity\n$etag")
            body.contentLength().takeIf { it >= 0 }
        }
        var received = if (append) offset else 0L
        progress(received, total)
        partial.outputStreamFor(append).use { output ->
            val input = body.byteStream()
            val bytes = ByteArray(64 * 1024)
            var reported = received
            while (true) {
                val count = input.read(bytes)
                if (count < 0) break
                received += count
                if (received > (total ?: MAX_APK_BYTES)) {
                    clearPartial()
                    throw UpdateProblem(R.string.update_apk_invalid)
                }
                output.write(bytes, 0, count)
                if (received - reported >= 1024 * 1024) {
                    progress(received, total)
                    reported = received
                }
            }
        }
        if (total != null && received != total) throw IOException("Incomplete APK transfer")
        true
    }

    private fun get(
        source: UpdateSource,
        url: HttpUrl,
        token: String,
        offset: Long = 0,
        validator: String? = null,
    ): Response {
        require(url.isHttps)
        val authenticated = source.origin == UpdateOrigin.Server
        var request = Request.Builder().url(url).header("Accept-Encoding", "identity").apply {
            if (offset > 0 && validator != null) {
                header("Range", "bytes=$offset-")
                header("If-Range", validator)
            }
            if (authenticated && token.isNotEmpty() && url.sameOrigin(source.feed) && !url.isGitHubHost()) {
                header("Authorization", "Bearer $token")
            }
        }.build()
        repeat(20) {
            val response = updateClient.newCall(request).execute()
            if (authenticated || response.code !in listOf(300, 301, 302, 303, 307, 308)) return response
            val location = response.header("Location") ?: return response
            response.close()
            val redirect = request.url.resolve(location)
            if (redirect?.isHttps != true) throw IOException("Update redirect must use HTTPS")
            request = request.newBuilder().url(redirect).build()
        }
        throw IOException("Too many update redirects")
    }

    private fun requireSuccess(response: Response, fallback: Int) {
        if (!response.isSuccessful) throw UpdateProblem(when (response.code) {
            401, 403 -> R.string.update_auth_failed
            404 -> R.string.update_feed_missing
            409 -> R.string.update_incompatible
            else -> fallback
        })
    }
}

private fun File.outputStreamFor(append: Boolean) = java.io.FileOutputStream(this, append)

private fun HttpUrl.sameOrigin(other: HttpUrl) = scheme == other.scheme && host == other.host && port == other.port

private fun HttpUrl.isGitHubHost(): Boolean {
    val domain = host.trimEnd('.')
    return domain == "github.com" || domain.endsWith(".github.com") ||
        domain == "githubusercontent.com" || domain.endsWith(".githubusercontent.com")
}

internal fun strongEtag(value: String?): String? =
    value?.takeIf { it.length in 2..256 && it.first() == '"' && it.last() == '"' }

internal fun resumedLength(range: String?, offset: Long, length: Long): Long? {
    val match = Regex("bytes ([0-9]+)-([0-9]+)/([0-9]+)").matchEntire(range.orEmpty()) ?: return null
    val (start, end, total) = match.groupValues.drop(1).map { it.toLongOrNull() ?: return null }
    return total.takeIf { start == offset && total in 1..MAX_APK_BYTES && start < total &&
        end == total - 1 && (length < 0 || length == total - start) }
}

internal fun sameApkSigners(candidate: Array<Signature>?, installed: Array<Signature>?): Boolean =
    !candidate.isNullOrEmpty() && !installed.isNullOrEmpty() && candidate.toSet() == installed.toSet()
