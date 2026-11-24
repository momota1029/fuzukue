package dev.labgui.remote

import android.content.Context
import androidx.core.content.FileProvider
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import okhttp3.HttpUrl
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.ResponseBody
import java.io.File
import java.io.IOException
import java.util.concurrent.TimeUnit

private const val MAX_MANIFEST_BYTES = 32_768L

internal data class UpdateState(val url: String = "", val status: UpdateStatus = UpdateStatus.Idle) {
    val busy get() = status == UpdateStatus.Checking || status == UpdateStatus.Downloading
}

internal sealed interface UpdateStatus {
    data object Idle : UpdateStatus
    data object Checking : UpdateStatus
    data object Current : UpdateStatus
    data object Downloading : UpdateStatus
    data class Available(val release: UpdateRelease, val error: Int? = null) : UpdateStatus
    data class Ready(val file: File) : UpdateStatus
    data class Failed(val error: Int) : UpdateStatus
}

class UpdateFileProvider : FileProvider()

// This state belongs to the application so a rotation keeps the check or download in progress.
internal class AndroidUpdater(private val context: Context) {
    private val prefs = context.getSharedPreferences("updates", Context.MODE_PRIVATE)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val client = OkHttpClient.Builder()
        // GitHub redirects to its asset host. Disallow redirects that change the scheme.
        .followSslRedirects(false)
        .callTimeout(5, TimeUnit.MINUTES)
        .build()
    private val mutableState = MutableStateFlow(UpdateState(prefs.getString("url", "").orEmpty()))
    val state = mutableState.asStateFlow()

    fun check(input: String) {
        if (state.value.busy) return
        val url = input.trim()
        val feed = httpsUpdateUrl(url)
        if (feed == null) {
            if (url.isEmpty()) prefs.edit().putString("url", "").apply()
            val error = if (url.isEmpty()) R.string.update_url_missing else R.string.update_url_invalid
            mutableState.value = UpdateState(url, UpdateStatus.Failed(error))
            return
        }

        prefs.edit().putString("url", url).apply()
        mutableState.value = UpdateState(url, UpdateStatus.Checking)
        scope.launch {
            val status = try {
                val release = withContext(Dispatchers.IO) {
                    get(feed) { body ->
                        val source = body.source()
                        require(!source.request(MAX_MANIFEST_BYTES + 1))
                        UpdateRelease.parse(source.readUtf8(), context.packageName)
                    }
                }
                if (release.isNewerThan(BuildConfig.VERSION_CODE)) UpdateStatus.Available(release)
                else UpdateStatus.Current
            } catch (_: IOException) {
                UpdateStatus.Failed(R.string.update_check_failed)
            } catch (_: IllegalArgumentException) {
                UpdateStatus.Failed(R.string.update_manifest_invalid)
            }
            mutableState.value = UpdateState(url, status)
        }
    }

    fun download() {
        val (url, status) = state.value
        if (status !is UpdateStatus.Available) return
        val release = status.release
        val feed = checkNotNull(httpsUpdateUrl(url))
        mutableState.value = UpdateState(url, UpdateStatus.Downloading)
        scope.launch {
            val result = try {
                withContext(Dispatchers.IO) { download(release, release.apkUrl(feed)) }
                    ?: UpdateStatus.Available(release, R.string.update_apk_invalid)
            } catch (_: IOException) {
                UpdateStatus.Available(release, R.string.update_download_failed)
            } catch (_: IllegalArgumentException) {
                UpdateStatus.Available(release, R.string.update_apk_invalid)
            }
            mutableState.value = UpdateState(url, result)
        }
    }

    fun apkForInstall(): File? {
        val (url, status) = state.value
        if (status !is UpdateStatus.Ready) return null
        if (status.file.isFile) return status.file
        mutableState.value = UpdateState(url, UpdateStatus.Failed(R.string.update_apk_missing))
        return null
    }

    @Suppress("DEPRECATION") // The integer flags overload also works on API 29–32.
    private fun download(release: UpdateRelease, url: HttpUrl): UpdateStatus.Ready? {
        val directory = File(context.cacheDir, "updates")
        if (!directory.isDirectory && !directory.mkdirs()) throw IOException("Cannot create update cache")
        val apk = File.createTempFile("fuzukue-", ".apk", directory)
        try {
            val matches = get(url) { body ->
                apk.outputStream().use { output -> copyAndVerifySha256(body.byteStream(), output, release.sha256) }
            }
            if (!matches) return null
            val archive = context.packageManager.getPackageArchiveInfo(apk.absolutePath, 0) ?: return null
            if (archive.packageName != context.packageName || archive.longVersionCode != release.versionCode.toLong()) {
                return null
            }
            val verified = File(directory, "${release.sha256.lowercase()}.apk")
            if (!apk.renameTo(verified)) throw IOException("Cannot store the verified APK")
            return UpdateStatus.Ready(verified)
        } finally {
            apk.delete()
        }
    }

    private fun <T> get(url: HttpUrl, read: (ResponseBody) -> T): T {
        require(url.isHttps)
        val request = Request.Builder().url(url).build()
        return client.newCall(request).execute().use { response ->
            if (!response.isSuccessful) throw IOException("Update request failed: ${response.code}")
            read(response.body)
        }
    }
}
