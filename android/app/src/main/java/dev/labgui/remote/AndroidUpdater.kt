package dev.labgui.remote

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import javax.net.ssl.SSLException

private const val CHECK_INTERVAL = 60 * 60 * 1000L
private const val RETRY_INTERVAL = 15 * 60 * 1000L
private const val SESSION_ID = "install_session"
private const val SESSION_VERSION = "install_version"
private const val SESSION_PENDING = "install_pending"
private const val SESSION_RETRY = "install_retry"

internal data class UpdateState(
    val url: String = "",
    val status: UpdateStatus = UpdateStatus.Idle,
    val auto: Boolean = false,
    val channel: UpdateChannel = UpdateChannel.Beta,
    val origin: UpdateOrigin = if (url.isBlank()) UpdateOrigin.Server else UpdateOrigin.Custom,
    val canInstallFromSource: Boolean = false,
    val applyBlocked: Boolean = false,
    val feedOrigin: UpdateOrigin? = null,
) {
    val busy get() = status == UpdateStatus.Checking || status is UpdateStatus.Downloading || installing
    val installing get() = status is UpdateStatus.Installing || status is UpdateStatus.Confirmation
}

internal sealed interface UpdateStatus {
    data object Idle : UpdateStatus
    data object Checking : UpdateStatus
    data object Current : UpdateStatus
    data class Downloading(val release: UpdateRelease, val bytes: Long = 0, val total: Long? = null) : UpdateStatus
    data class Available(val release: UpdateRelease, val error: Int? = null) : UpdateStatus
    data class Ready(val release: UpdateRelease, val file: File, val error: Int? = null) : UpdateStatus
    data class Installing(val release: UpdateRelease) : UpdateStatus
    data class Confirmation(val release: UpdateRelease) : UpdateStatus
    data class Failed(val error: Int) : UpdateStatus
}

// This state belongs to the application so a rotation keeps the check or download in progress.
internal class AndroidUpdater(
    context: Context,
    private val settings: ConnectionSettings = ConnectionSettings(context),
) {
    private val prefs = context.getSharedPreferences("updates", Context.MODE_PRIVATE)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val disk = Mutex()
    private val transfer = UpdateDownload(context)
    private val installer = UpdateInstaller(context)
    private val packageName = context.packageName
    private val mutableState = MutableStateFlow(loadSettings())
    val state = mutableState.asStateFlow()
    private var connection = settings.url
    private var generation = 0L
    private var nextCheck = 0L
    private var session = prefs.getInt(SESSION_ID, -1)
    private var confirmation: Intent? = null
    private var availableSource: UpdateSource? = null

    init {
        installer.removeOrphanSessions(session)
        restore()
    }

    private fun loadSettings(): UpdateState {
        val channel = UpdateChannel.entries.firstOrNull { it.name == prefs.getString("channel", null) }
            ?: UpdateChannel.Beta
        val url = customUrl(channel)
        val origin = UpdateOrigin.entries.firstOrNull { it.name == prefs.getString("origin", null) }
            ?: if (url.isBlank()) UpdateOrigin.Server else UpdateOrigin.Custom
        return UpdateState(url, auto = prefs.getBoolean("auto", false), channel = channel,
            origin = if (channel == UpdateChannel.Dev) UpdateOrigin.Server else origin,
            canInstallFromSource = installer.allowed)
    }

    private fun customUrl(channel: UpdateChannel): String = prefs.getString("url_${channel.path}", null)
        ?: prefs.getString("url", "").orEmpty().takeIf { channel == UpdateChannel.Beta }.orEmpty()

    private fun source(current: UpdateState = state.value): UpdateSource? = when (current.origin) {
        UpdateOrigin.Server -> serverUpdateSource(connection, current.channel)
        UpdateOrigin.Custom -> httpsUpdateUrl(current.url)?.let { UpdateSource(it, current.origin, current.channel) }
    }

    private fun candidateKey(source: UpdateSource) = "${socketUrl(connection).orEmpty()}\n${source.key}"

    private fun fallbackSource(current: UpdateState = state.value): UpdateSource? =
        if (current.origin == UpdateOrigin.Server && current.channel != UpdateChannel.Dev) {
            httpsUpdateUrl(current.url)?.let { UpdateSource(it, UpdateOrigin.Custom, current.channel) }
        } else null

    fun setAuto(value: Boolean) {
        prefs.edit().putBoolean("auto", value).apply()
        mutableState.value = state.value.copy(auto = value)
        nextCheck = 0
    }

    fun setChannel(channel: UpdateChannel) {
        if (state.value.installing || channel == state.value.channel) return
        prefs.edit().putString("channel", channel.name).apply()
        resetSource(loadSettings())
    }

    fun setOrigin(origin: UpdateOrigin) {
        if (state.value.installing || state.value.channel == UpdateChannel.Dev || origin == state.value.origin) return
        prefs.edit().putString("origin", origin.name).apply()
        resetSource(loadSettings())
    }

    fun connectionChanged() {
        if (settings.url == connection) return
        connection = settings.url
        if (state.value.status is UpdateStatus.Confirmation) cancelConfirmation()
        if (state.value.installing && session >= 0) return // A committed install will report its result.
        resetSource(state.value)
    }

    private fun resetSource(next: UpdateState) {
        generation++
        nextCheck = 0
        availableSource = null
        prefs.edit().clearCandidate().remove(SESSION_RETRY).apply()
        mutableState.value = next.copy(status = UpdateStatus.Idle, feedOrigin = null)
    }

    fun check(input: String, protocol: UpdateProtocol? = null) {
        if (state.value.busy) return
        if (state.value.origin == UpdateOrigin.Custom) {
            val url = input.trim()
            prefs.edit().putString("url_${state.value.channel.path}", url).apply()
            if (url != state.value.url) resetSource(state.value.copy(url = url))
        }
        prefs.edit().remove(SESSION_RETRY).apply()
        check(protocol)
    }

    private fun check(protocol: UpdateProtocol?) {
        val source = source() ?: run {
            val reason = when {
                state.value.origin == UpdateOrigin.Server -> R.string.update_server_https
                state.value.url.isBlank() -> R.string.update_url_missing
                else -> R.string.update_url_invalid
            }
            nextCheck = System.currentTimeMillis() + RETRY_INTERVAL
            show(UpdateStatus.Failed(reason))
            return
        }
        val key = generation
        val fallback = fallbackSource().takeIf { protocol != null }
        prefs.edit().clearCandidate().apply()
        availableSource = null
        mutableState.value = state.value.copy(feedOrigin = null)
        show(UpdateStatus.Checking)
        scope.launch {
            var checkedSource = source
            val status = try {
                val (actual, release) = withContext(Dispatchers.IO) {
                    try {
                        source to transfer.check(source, token(source), protocol)
                    } catch (error: IOException) {
                        if (error is UpdateProblem || error is SSLException || fallback == null) throw error
                        fallback to transfer.check(fallback, "", protocol)
                    }
                }
                checkedSource = actual
                if (release.versionCode > installer.installedVersion()) UpdateStatus.Available(release)
                else UpdateStatus.Current
            } catch (error: UpdateProblem) {
                UpdateStatus.Failed(error.label)
            } catch (_: IOException) {
                UpdateStatus.Failed(R.string.update_check_failed)
            } catch (_: IllegalArgumentException) {
                UpdateStatus.Failed(R.string.update_manifest_invalid)
            }
            if (key != generation) return@launch
            nextCheck = System.currentTimeMillis() +
                if (status is UpdateStatus.Failed) RETRY_INTERVAL else CHECK_INTERVAL
            availableSource = checkedSource.takeIf { status is UpdateStatus.Available }
            mutableState.value = state.value.copy(status = status, feedOrigin = checkedSource.origin)
        }
    }

    fun download() {
        val available = state.value.status as? UpdateStatus.Available ?: return
        val source = availableSource ?: return
        val configuredSource = source() ?: return
        val release = available.release
        val key = generation
        show(UpdateStatus.Downloading(release))
        scope.launch {
            val status = try {
                val token = token(source)
                val file = withContext(Dispatchers.IO) {
                    disk.withLock {
                        transfer.download(source, release, token) { bytes, total ->
                            scope.launch {
                                if (key == generation && state.value.status is UpdateStatus.Downloading) {
                                    show(UpdateStatus.Downloading(release, bytes, total))
                                }
                            }
                        }
                    }
                }
                UpdateStatus.Ready(release, file)
            } catch (error: UpdateProblem) {
                if (error.label == R.string.update_apk_invalid) UpdateStatus.Failed(error.label)
                else UpdateStatus.Available(release, error.label)
            } catch (_: IOException) {
                UpdateStatus.Available(release, R.string.update_download_failed)
            } catch (_: IllegalArgumentException) {
                UpdateStatus.Failed(R.string.update_apk_invalid)
            }
            if (key != generation) return@launch
            if (status is UpdateStatus.Ready) {
                prefs.edit().putString("candidate", Protocol.json.encodeToString(UpdateRelease.serializer(), release))
                    .putString("candidate_source", candidateKey(configuredSource))
                    .putString("candidate_feed", source.key).apply()
            } else nextCheck = System.currentTimeMillis() + RETRY_INTERVAL
            show(status)
        }
    }

    private fun token(source: UpdateSource): String {
        if (source.origin != UpdateOrigin.Server || source != serverUpdateSource(settings.url, source.channel)) return ""
        return runCatching { settings.token() }.getOrElse { throw UpdateProblem(R.string.error_token_read) }
    }

    fun refreshInstallState(blocked: Boolean) {
        mutableState.value = state.value.copy(canInstallFromSource = installer.allowed, applyBlocked = blocked)
    }

    fun autoTick(
        protocol: UpdateProtocol?, mayApply: (UpdateRelease) -> Boolean, checkpoint: (UpdateRelease) -> Boolean,
    ) {
        val current = state.value
        if (!current.auto || current.busy || prefs.contains(SESSION_RETRY)) return
        val now = System.currentTimeMillis()
        when (val status = current.status) {
            is UpdateStatus.Ready -> if (Build.VERSION.SDK_INT >= 31 && current.canInstallFromSource &&
                autoCompatible(status.release, protocol) && mayApply(status.release)) {
                install(unattended = true, mayApply = mayApply, checkpoint = checkpoint)
            }
            is UpdateStatus.Available -> if (autoCompatible(status.release, protocol) &&
                (status.error == null || now >= nextCheck)) download()
            else -> if (now >= nextCheck && (current.origin == UpdateOrigin.Server || protocol != null)) check(protocol)
        }
    }

    private fun autoCompatible(release: UpdateRelease, protocol: UpdateProtocol?) =
        release.maximumProtocol != null && (protocol?.let { supportsConnectedServer(release, it) }
            ?: (availableSource?.origin == UpdateOrigin.Server))

    fun supportsConnectedServer(release: UpdateRelease, protocol: UpdateProtocol): Boolean =
        // A Welcome can report only the negotiated version. The authenticated feed also
        // checks the running server's upper bound and is re-read before committing.
        if (availableSource?.origin == UpdateOrigin.Server) {
            release.maximumProtocol?.let { it >= protocol.minimum } == true
        } else release.supports(protocol)

    fun install(
        unattended: Boolean = false, mayApply: (UpdateRelease) -> Boolean, checkpoint: (UpdateRelease) -> Boolean,
    ) {
        val ready = state.value.status as? UpdateStatus.Ready ?: return
        val source = availableSource ?: return
        if (!installer.allowed || !mayApply(ready.release)) return
        val key = generation
        show(UpdateStatus.Installing(ready.release))
        scope.launch {
            var id = -1
            try {
                id = withContext(Dispatchers.IO) {
                    disk.withLock {
                        val latest = transfer.check(source, token(source), null)
                        if (latest != ready.release) throw UpdateProblem(R.string.update_candidate_changed)
                        if (!transfer.verify(ready.file, ready.release)) throw UpdateProblem(R.string.update_apk_invalid)
                        installer.stage(ready.file, unattended)
                    }
                }
                // The user, conversation and installed version may all have changed during staging.
                if (key != generation || !mayApply(ready.release) || !installer.allowed ||
                    (unattended && !state.value.auto) || ready.release.versionCode <= installer.installedVersion()
                ) {
                    installer.abandon(id)
                    if (key == generation) show(ready)
                    return@launch
                }
                session = id
                check(prefs.edit().putInt(SESSION_ID, id).putInt(SESSION_VERSION, ready.release.versionCode)
                    .remove(SESSION_PENDING).remove(SESSION_RETRY).commit())
                check(checkpoint(ready.release))
                installer.commit(id)
            } catch (error: Exception) {
                if (id >= 0) installer.abandon(id)
                if (key == generation) {
                    if (error is UpdateProblem && error.label == R.string.update_apk_invalid) {
                        ready.file.delete()
                    }
                    if (error is UpdateProblem && error.label in listOf(
                            R.string.update_candidate_changed, R.string.update_auth_failed,
                            R.string.update_incompatible, R.string.update_feed_missing,
                        )
                    ) {
                        prefs.edit().clearCandidate().apply()
                        nextCheck = System.currentTimeMillis() + RETRY_INTERVAL
                        show(UpdateStatus.Failed(error.label))
                    } else finishInstall((error as? UpdateProblem)?.label ?: R.string.update_install_failed)
                }
            }
        }
    }

    fun confirm(mayApply: (UpdateRelease) -> Boolean, launch: (Intent) -> Unit) {
        val status = state.value.status as? UpdateStatus.Confirmation ?: return
        if (installer.allowed && mayApply(status.release) && status.release.versionCode > installer.installedVersion()) {
            confirmation?.let(launch)
        }
    }

    fun cancelConfirmation() {
        if (state.value.status !is UpdateStatus.Confirmation) return
        if (session >= 0) installer.abandon(session)
        finishInstall(R.string.update_install_cancelled)
    }

    @Suppress("DEPRECATION") // Intent's typed overload starts at API 33.
    fun onInstallResult(intent: Intent) {
        if (session < 0 || intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1) != session) return
        val release = (state.value.status as? UpdateStatus.Installing)?.release
            ?: (state.value.status as? UpdateStatus.Confirmation)?.release
            ?: savedCandidate() ?: return
        when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                confirmation = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
                if (confirmation == null) {
                    installer.abandon(session)
                    finishInstall(R.string.update_install_failed)
                } else {
                    prefs.edit().putBoolean(SESSION_PENDING, true).apply()
                    show(UpdateStatus.Confirmation(release))
                }
            }
            PackageInstaller.STATUS_SUCCESS -> finishInstall(null)
            PackageInstaller.STATUS_FAILURE_ABORTED -> finishInstall(R.string.update_install_cancelled)
            PackageInstaller.STATUS_FAILURE_STORAGE -> finishInstall(R.string.update_install_storage)
            else -> finishInstall(R.string.update_install_failed)
        }
    }

    private fun savedCandidate(): UpdateRelease? {
        val source = source() ?: return null
        if (prefs.getString("candidate_source", null) != candidateKey(source)) return null
        val savedFeed = prefs.getString("candidate_feed", null)
        availableSource = listOfNotNull(source, fallbackSource()).find { it.key == savedFeed } ?: return null
        val text = prefs.getString("candidate", null) ?: return null
        return runCatching { UpdateRelease.parse(text, packageName) }.getOrNull()
    }

    private fun restore() {
        val candidate = savedCandidate()
        val installed = installer.installedVersion()
        if (session >= 0) {
            val expected = prefs.getInt(SESSION_VERSION, 0)
            if (expected > 0 && installed >= expected) {
                finishInstall(null)
            } else if (candidate == null || candidate.versionCode != expected ||
                prefs.getBoolean(SESSION_PENDING, false) || !installer.exists(session)) {
                installer.abandon(session)
                finishInstall(R.string.update_install_failed)
            } else show(UpdateStatus.Installing(candidate))
        } else if (candidate != null && candidate.versionCode > installed) {
            val file = transfer.verifiedFile(candidate)
            val retry = prefs.getInt(SESSION_RETRY, 0).takeIf { it != 0 }
            if (file.isFile) mutableState.value = state.value.copy(
                status = UpdateStatus.Ready(candidate, file, retry), feedOrigin = availableSource?.origin,
            )
        }
    }

    private fun finishInstall(error: Int?) {
        val candidate = savedCandidate()
        val editor = prefs.edit().remove(SESSION_ID).remove(SESSION_VERSION).remove(SESSION_PENDING)
        if (error != null) editor.putInt(SESSION_RETRY, error)
        else editor.remove(SESSION_RETRY).clearCandidate()
        editor.apply()
        session = -1
        confirmation = null
        val retry = candidate?.let(transfer::verifiedFile)
        show(when {
            error == null -> UpdateStatus.Current
            candidate != null && retry?.isFile == true -> UpdateStatus.Ready(candidate, retry, error)
            candidate != null -> UpdateStatus.Available(candidate, error)
            else -> UpdateStatus.Failed(error)
        })
    }

    private fun show(status: UpdateStatus) {
        mutableState.value = state.value.copy(status = status)
    }

    private fun android.content.SharedPreferences.Editor.clearCandidate() =
        remove("candidate").remove("candidate_source").remove("candidate_feed")
}
