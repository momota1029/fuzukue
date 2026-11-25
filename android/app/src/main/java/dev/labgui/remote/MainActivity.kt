package dev.labgui.remote

import android.Manifest
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.provider.Settings
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.lifecycle.Lifecycle
import kotlinx.coroutines.delay

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
    private var pendingApprovalDock = false
    private val openApprovalDock = mutableStateOf<Pair<ConversationTarget, Int>?>(null)
    private var requestedNoticeWorkspace: String? = null
    private val resumed = mutableStateOf(false)
    private var lastInteraction = 0L
    private var restoreWorkspace: String? = null
    private var resumingAfterUpdate = false
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
        if (savedInstanceState == null) {
            app.settings.takeUpdateResume()?.let { (workspace, selection) ->
                resumingAfterUpdate = true
                if (workspace != null && selection != null) pendingNotice.value = ConversationTarget(workspace, selection)
                else restoreWorkspace = workspace
                connectFromIntent()
            }
            openIncoming(intent)
        } else openNotice(intent)
        setContent {
            val state by remote.state.collectAsState()
            val updates by app.updater.state.collectAsState()
            LaunchedEffect(state.canSend, state.workspace, state.changingWorkspace) {
                if (!state.canSend || !resumingAfterUpdate) return@LaunchedEffect
                resumingAfterUpdate = false
                val workspace = restoreWorkspace
                restoreWorkspace = null
                if (workspace != null && state.workspace != workspace) remote.switchWorkspace(workspace)
                if (pendingNotice.value == null) openConversation.value++
            }
            LaunchedEffect(
                pendingNotice.value, state.phase, state.workspace, state.threads, state.changingWorkspace,
                state.approvalDock.isLoading, state.approvalDock.pending,
            ) {
                val target = pendingNotice.value ?: return@LaunchedEffect
                if (!state.canSend) return@LaunchedEffect
                if (target.workspace == state.workspace) {
                    val knownThread = state.threads.any { it.id == target.session }
                    val queuedThread = pendingApprovalDock && state.approvalDock.pending.values.any { it.thread == target.session }
                    if (!knownThread && !queuedThread && pendingApprovalDock && state.approvalDock.isLoading) {
                        return@LaunchedEffect
                    }
                    pendingNotice.value = null
                    if (knownThread || queuedThread) {
                        remote.selectThread(target.session)
                        openConversation.value++
                        openApprovalDock.value = if (pendingApprovalDock) target to openConversation.value else null
                    }
                } else if (requestedNoticeWorkspace != target.workspace) {
                    requestedNoticeWorkspace = target.workspace
                    remote.switchWorkspace(target.workspace)
                } else {
                    pendingNotice.value = null
                }
            }
            LaunchedEffect(resumed.value, state.phase, updates.status, updates.auto, updates.channel, updates.origin) {
                while (resumed.value) {
                    val release = when (val status = updates.status) {
                        is UpdateStatus.Ready -> status.release
                        is UpdateStatus.Confirmation -> status.release
                        else -> null
                    }
                    app.updater.refreshInstallState(release != null && !mayApply(release, manual = true))
                    app.updater.autoTick(remote.updateProtocol, { mayApply(it, manual = false) }, ::checkpointUpdate)
                    delay(if (updates.status is UpdateStatus.Ready || updates.status is UpdateStatus.Confirmation) 2_000 else 30_000)
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
                    openApprovalDock = openApprovalDock.value,
                    updates = updates,
                    drafts = app.drafts,
                    installError = installError.value,
                    onCheckUpdate = {
                        installError.value = null
                        app.updater.check(it, remote.updateProtocol)
                    },
                    onDownloadUpdate = app.updater::download,
                    onInstallUpdate = ::installUpdate,
                    onAllowUpdate = ::openUpdatePermission,
                    onCancelUpdate = app.updater::cancelConfirmation,
                    onAutoUpdate = app.updater::setAuto,
                    onUpdateChannel = app.updater::setChannel,
                    onUpdateOrigin = app.updater::setOrigin,
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
                            app.updater.connectionChanged()
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
                    onRequestJob = remote::requestThreadJob,
                    onNewThread = remote::newThread,
                    onSend = remote::prompt,
                    onStopTurn = remote::interrupt,
                    onStopWorker = remote::stopWorker,
                    onResolve = remote::resolve,
                    onDecideQueuedApprovals = remote::decideQueuedApprovals,
                    onDismissQueuedDecision = remote::dismissQueuedDecision,
                    onRetryApprovalQueue = remote::retryApprovalQueue,
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
            (resumingAfterUpdate || pendingNotice.value != null || remote.attachments.view.value.share != null)
        ) syncService()
    }

    override fun onStop() {
        remote.setVisible(false)
        super.onStop()
    }

    override fun onResume() {
        super.onResume()
        resumed.value = true
    }

    override fun onPause() {
        resumed.value = false
        super.onPause()
    }

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        lastInteraction = SystemClock.elapsedRealtime()
        return super.dispatchTouchEvent(event)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        lastInteraction = SystemClock.elapsedRealtime()
        return super.dispatchKeyEvent(event)
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
        pendingApprovalDock = intent.getBooleanExtra(EXTRA_APPROVAL_DOCK, false)
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

    private fun canApplyUpdate(manual: Boolean): Boolean =
        resumed.value && hasWindowFocus() && pendingNotice.value == null && restoreWorkspace == null &&
            !app.drafts.editing && remote.canApplyAppUpdate(allowProtocolMismatch = manual) &&
            (manual || (SystemClock.elapsedRealtime() - lastInteraction >= 2_000 &&
                ViewCompat.getRootWindowInsets(window.decorView)?.isVisible(WindowInsetsCompat.Type.ime()) != true))

    private fun mayApply(release: UpdateRelease, manual: Boolean): Boolean =
        canApplyUpdate(manual) &&
            remote.updateProtocol?.let { app.updater.supportsConnectedServer(release, it) } == true

    private fun checkpointUpdate(release: UpdateRelease): Boolean {
        val state = remote.state.value
        return app.drafts.flush() && app.settings.saveUpdateResume(release.versionCode, state.workspace, state.selection)
    }

    private fun installUpdate() {
        installError.value = null
        val mayApply: (UpdateRelease) -> Boolean = { mayApply(it, manual = true) }
        when (val status = app.updater.state.value.status) {
            is UpdateStatus.Confirmation -> app.updater.confirm(mayApply) { intent ->
                if (checkpointUpdate(status.release)) launchInstaller(intent)
                else installError.value = getString(R.string.update_install_failed)
            }
            is UpdateStatus.Ready -> app.updater.install(mayApply = mayApply, checkpoint = ::checkpointUpdate)
            else -> Unit
        }
    }

    private fun openUpdatePermission() {
        installError.value = null
        launchInstaller(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.fromParts("package", packageName, null)))
    }

    private fun launchInstaller(intent: Intent) {
        try {
            startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            installError.value = getString(R.string.update_installer_unavailable)
        } catch (_: SecurityException) {
            installError.value = getString(R.string.update_installer_unavailable)
        }
    }
}
