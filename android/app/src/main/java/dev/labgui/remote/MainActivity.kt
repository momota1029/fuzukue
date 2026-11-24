package dev.labgui.remote

import android.Manifest
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.core.content.FileProvider
import androidx.lifecycle.Lifecycle

internal fun updateAuthority(context: Context) = "${context.packageName}.updates"

class MainActivity : ComponentActivity() {
    private val app get() = application as RemoteApplication
    private val remote get() = app.repository
    private val background = mutableStateOf(false)
    private val workerNotices = mutableStateOf(false)
    private val showPlanAboveConversation = mutableStateOf(false)
    private val serviceError = mutableStateOf<String?>(null)
    private val installError = mutableStateOf<String?>(null)
    private val openConversation = mutableStateOf(0)
    private val pendingNotice = mutableStateOf<ConversationTarget?>(null)
    private var requestedNoticeWorkspace: String? = null
    private var openInstallerAfterDownload = false
    private val permission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        serviceError.value = if (granted) null else getString(R.string.error_notification_permission)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        background.value = app.settings.background
        workerNotices.value = app.settings.workerNotices
        showPlanAboveConversation.value = app.settings.showPlanAboveConversation
        val loadedToken = runCatching { app.settings.token() }
        if (savedInstanceState == null) openIncoming(intent) else openNotice(intent)
        setContent {
            val state by remote.state.collectAsState()
            val updates by app.updater.state.collectAsState()
            LaunchedEffect(pendingNotice.value, state.phase, state.workspace, state.threads, state.changingWorkspace) {
                val target = pendingNotice.value ?: return@LaunchedEffect
                if (!state.canSend) return@LaunchedEffect
                if (target.workspace == state.workspace) {
                    pendingNotice.value = null
                    if (state.threads.any { it.id == target.session }) {
                        remote.selectThread(target.session)
                        openConversation.value++
                    }
                } else if (requestedNoticeWorkspace != target.workspace) {
                    requestedNoticeWorkspace = target.workspace
                    remote.switchWorkspace(target.workspace)
                } else {
                    pendingNotice.value = null
                }
            }
            LaunchedEffect(updates.status) {
                if (openInstallerAfterDownload && updates.status != UpdateStatus.Downloading) {
                    openInstallerAfterDownload = false
                    if (updates.status is UpdateStatus.Ready && lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) {
                        installUpdate()
                    }
                }
            }
            RemoteTheme {
                RemoteApp(
                    state = state,
                    initialUrl = app.settings.url,
                    initialToken = loadedToken.getOrDefault(""),
                    settingsError = serviceError.value ?: if (loadedToken.isFailure && !remote.wantsConnection) {
                        getString(R.string.error_token_read)
                    } else null,
                    background = background.value,
                    workerNotices = workerNotices.value,
                    showPlanAboveConversation = showPlanAboveConversation.value,
                    openConversation = openConversation.value,
                    updates = updates,
                    installError = installError.value,
                    onCheckUpdate = {
                        installError.value = null
                        app.updater.check(it)
                    },
                    onDownloadUpdate = {
                        openInstallerAfterDownload = true
                        app.updater.download()
                    },
                    onInstallUpdate = ::installUpdate,
                    onBackground = { checked ->
                        app.settings.background = checked
                        background.value = checked
                        serviceError.value = null
                        syncService()
                        if (checked) requestNotifications()
                    },
                    onWorkerNotices = { checked ->
                        app.settings.workerNotices = checked
                        workerNotices.value = checked
                    },
                    onShowPlanAboveConversation = { checked ->
                        app.settings.showPlanAboveConversation = checked
                        showPlanAboveConversation.value = checked
                    },
                    onConnect = { url, token ->
                        if (remote.connect(url, token)) {
                            syncService()
                            if (background.value) requestNotifications()
                        }
                    },
                    onDisconnect = {
                        remote.disconnect()
                        syncService()
                    },
                    onWorkspace = remote::switchWorkspace,
                    onThread = remote::selectThread,
                    onOlderHistory = remote::requestOlderHistory,
                    onNewThread = remote::newThread,
                    onSend = remote::prompt,
                    onStopTurn = remote::interrupt,
                    onStopWorker = remote::stopWorker,
                    onResolve = remote::resolve,
                    onRememberShellApproval = remote::rememberShellApproval,
                    onRevokeShellApproval = remote::revokeShellApproval,
                    onWorkerApprovalPolicy = remote::setWorkerApprovalPolicy,
                    attachments = remote.attachments,
                )
            }
        }
    }

    override fun onStart() {
        super.onStart()
        remote.setVisible(true)
        if (background.value && remote.wantsConnection &&
            (pendingNotice.value != null || remote.attachments.view.value.share != null)
        ) syncService()
    }

    override fun onStop() {
        remote.setVisible(false)
        super.onStop()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        openIncoming(intent)
    }

    private fun openIncoming(intent: Intent) {
        if (intent.action == Intent.ACTION_SEND || intent.action == Intent.ACTION_SEND_MULTIPLE) {
            val share = readSharedContent(intent) ?: return
            remote.attachments.offerShare(share)
            connectFromIntent()
            if (remote.wantsConnection || remote.state.value.workspace != null) openConversation.value++
            if (background.value && remote.wantsConnection &&
                lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)
            ) syncService()
        } else openNotice(intent)
    }

    private fun openNotice(intent: Intent) {
        val workspace = intent.getStringExtra(EXTRA_WORKSPACE) ?: return
        val thread = intent.getStringExtra(EXTRA_SESSION) ?: return
        requestedNoticeWorkspace = null
        pendingNotice.value = ConversationTarget(workspace, thread)
        connectFromIntent()
    }

    private fun connectFromIntent() {
        if (!remote.wantsConnection && app.settings.url.isNotBlank()) {
            runCatching { app.settings.token() }.onSuccess { remote.connect(app.settings.url, it) }
        }
    }

    private fun syncService() {
        val intent = Intent(this, ConnectionService::class.java)
        if (!background.value || !remote.wantsConnection) {
            stopService(intent)
            return
        }
        try {
            startForegroundService(intent)
        } catch (_: IllegalStateException) {
            serviceError.value = getString(R.string.error_background_connection)
        }
    }

    private fun requestNotifications() {
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) permission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    @Suppress("DEPRECATION") // Android's installer activity prompts the user for confirmation.
    private fun installUpdate() {
        installError.value = null
        val apk = app.updater.apkForInstall() ?: return
        try {
            if (!packageManager.canRequestPackageInstalls()) {
                startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.fromParts("package", packageName, null)))
                return
            }
            val uri = FileProvider.getUriForFile(this, updateAuthority(this), apk)
            startActivity(Intent(Intent.ACTION_INSTALL_PACKAGE, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        } catch (_: ActivityNotFoundException) {
            installError.value = getString(R.string.update_installer_unavailable)
        } catch (_: SecurityException) {
            installError.value = getString(R.string.update_installer_unavailable)
        }
    }
}
