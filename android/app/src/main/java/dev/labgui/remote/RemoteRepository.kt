package dev.labgui.remote

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.util.concurrent.TimeUnit

private const val LOG_TAG = "RemoteRepository"

enum class ConnectionPhase(val label: Int) {
    Disconnected(R.string.connection_disconnected), Connecting(R.string.connection_connecting), Connected(R.string.connection_connected), Retrying(R.string.connection_retrying),
}

internal data class ConversationTarget(val workspace: String, val session: String)

internal data class PendingPrompt(
    val id: Long,
    val text: String,
    val sentAt: Long,
    val drainStartedAt: Long?,
    val paths: List<String> = emptyList(),
)

internal data class PromptOutbox(
    val messages: List<PendingPrompt>,
    val seenUserItems: Set<String>,
)

internal data class RemoteState(
    val phase: ConnectionPhase = ConnectionPhase.Disconnected,
    val workspace: String? = null,
    val workspaces: List<Workspace> = emptyList(),
    val threads: List<ThreadInfo> = emptyList(),
    val timelines: Map<String, Timeline> = emptyMap(),
    val approvals: Map<String, ServerEvent.ApprovalRequested> = emptyMap(),
    val approvalDock: ApprovalDockState = ApprovalDockState(),
    val workerApprovalPolicy: WorkerApprovalPolicy? = null,
    val shellApprovalRules: List<ShellApprovalRule>? = null,
    val resolving: Set<String> = emptySet(),
    val outboxes: Map<ConversationTarget, PromptOutbox> = emptyMap(),
    val waiting: ChatWaiting = ChatWaiting(),
    val drain: DrainStatus? = null,
    val heldConversations: Set<ConversationTarget> = emptySet(),
    val selection: String? = null,
    val changingWorkspace: Boolean = false,
    val creatingThread: Boolean = false,
    val error: String? = null,
    val updateRequired: Boolean = false,
) {
    val canSend: Boolean get() = phase == ConnectionPhase.Connected && !changingWorkspace

    fun timeline(session: String): Timeline = timelines[session] ?: Timeline()

    fun withTimeline(session: String, timeline: Timeline): RemoteState =
        if (timeline == this.timeline(session)) this else copy(timelines = timelines + (session to timeline))

    fun pendingPrompts(session: String): List<PendingPrompt> =
        outboxes[ConversationTarget(workspace.orEmpty(), session)]?.messages.orEmpty()

    fun hasHeldInput(session: String): Boolean =
        ConversationTarget(workspace.orEmpty(), session) in heldConversations

    fun isTurnRunning(session: String): Boolean =
        timelines[session]?.running == true && waiting.reason(session) == null
}

internal data class ApprovalNotice(val target: ConversationTarget, val requestId: String)

internal data class EventNotice(
    val title: String,
    val threadTitle: String,
    val target: ConversationTarget?,
    val approval: ApprovalNotice? = null,
    val openApprovalDock: Boolean = false,
)

// Notification buttons are reserved for short, server-checked Bash requests whose
// exact command and directory can be shown in the notification.
internal fun ServerEvent.ApprovalRequested.notificationDetails(): String? {
    val directory = shellDirectory ?: return null
    if (!summary.startsWith("Bash: ") || summary.length > 100 || directory.length > 120 ||
        summary.any(Char::isISOControl) || directory.any(Char::isISOControl)
    ) return null
    return "$summary\n$directory"
}

internal fun RemoteState.notificationApproval(approval: ApprovalNotice): ServerEvent.ApprovalRequested? {
    if (!canSend || workspace != approval.target.workspace || approval.requestId in approvalDock.pending) return null
    return approvals[approval.requestId]?.takeIf {
        it.session == approval.target.session && it.notificationDetails() != null
    }
}

// Activity, service, and all socket callbacks mutate state on the main looper.
internal class RemoteRepository(
    private val context: Context,
    private val settings: ConnectionSettings,
    private val client: WebSocket.Factory = OkHttpClient.Builder()
        .pingInterval(20, TimeUnit.SECONDS)
        .followRedirects(false)
        .followSslRedirects(false)
        .build(),
) {
    private val handler = Handler(Looper.getMainLooper())
    private val mutableState = MutableStateFlow(RemoteState())
    val state = mutableState.asStateFlow()
    private val mutableNotices = MutableSharedFlow<EventNotice>(extraBufferCapacity = 32)
    val notices = mutableNotices.asSharedFlow()

    private var socket: WebSocket? = null
    private var request: Request? = null
    private var connectedEndpoint: String? = null
    val wantsConnection: Boolean get() = request != null
    private var visible = false
    private var serviceRunning = false
    private var welcomed = false
    private var serverProtocol = 0
    var updateProtocol: UpdateProtocol? = null
        private set
    private var sentLocale: String? = null
    private var replaying = true
    private var selectionScoped = false
    private var sentTranscriptResume = false
    private var transcriptEpoch: String? = null
    private var transcriptThrough = 0uL
    private var nextHistoryRequest = 0uL
    private var approvalQueueSupported = false
    private var nextApprovalRequest = 0uL
    private var approvalSnapshotRequest: ULong? = null
    private var approvalQueueReady = false
    private val histories = mutableMapOf<String, TranscriptHistory>()
    private val transcriptReplay = mutableListOf<TranscriptUpdate>()
    private var initialThreads: ServerEvent.Threads? = null
    private var initialActivity: ServerEvent.ChatActivity? = null
    private var updateActive = false
    private val recentNotices = mutableMapOf<ConversationTarget?, Pair<Long, Boolean>>()
    private var receivedDrainStatus = false
    private var selectionToRestore: String? = null
    private var nextPromptId = 0L
    private var reconnectPromptLimit = 0L
    private var retryDelay = 1_000L
    private val retry = Runnable { maintainConnection() }
    private val helloTimeout = Runnable { connectionLost(context.getString(R.string.error_hello_timeout)) }
    private val resolved = mutableSetOf<String>()
    val attachments by lazy {
        Attachments(context, canUpload = ::canUpload, awaitUpload = ::awaitUpload, send = ::sendToWorkspace)
    }

    fun canApplyAppUpdate(allowProtocolMismatch: Boolean): Boolean {
        val current = state.value
        if (current.creatingThread || current.changingWorkspace || current.approvals.isNotEmpty() ||
            current.resolving.isNotEmpty() || current.approvalDock.pending.isNotEmpty() ||
            current.approvalDock.decisions.values.any { it.isSending } || current.outboxes.isNotEmpty() ||
            current.drain?.holdingInput == true || current.heldConversations.isNotEmpty()
        ) return false
        val attachments = attachments.view.value
        if (attachments.share != null || attachments.drafts.values.any {
                it.items.isNotEmpty() || it.sharedText != null
            }
        ) return false
        val selected = current.selection
        val timeline = selected?.let(current::timeline)
        if (timeline != null && (timeline.running || timeline.rows.values.any { it is TimelineRow.Job && it.ok == null })) {
            return false
        }
        if (allowProtocolMismatch && current.updateRequired && updateProtocol != null) return true
        return current.canSend && !replaying && (!approvalQueueSupported || approvalQueueReady) &&
            (!selectionScoped || selected == null || histories[selected]?.generation != null)
    }

    private fun canUpload(target: ConversationTarget): Boolean {
        val current = state.value
        return current.canSend && current.workspace == target.workspace &&
            current.threads.any { it.id == target.session }
    }

    private suspend fun awaitUpload(target: ConversationTarget): Boolean {
        // The system document picker or camera can stop the Activity and close its foreground socket.
        withTimeoutOrNull(15_000) {
            state.first {
                it.workspace != target.workspace || it.changingWorkspace ||
                    it.phase == ConnectionPhase.Connected || !wantsConnection
            }
        }
        return canUpload(target)
    }

    fun setVisible(value: Boolean) {
        visible = value
        maintainConnection()
    }

    fun setServiceRunning(value: Boolean) {
        serviceRunning = value
        maintainConnection()
    }

    fun connect(url: String, token: String): Boolean {
        val endpoint = socketUrl(url)
        if (endpoint == null) {
            mutableState.value = state.value.copy(error = context.getString(R.string.error_server_url))
            return false
        }
        val secret = token.trim()
        val next = runCatching {
            Request.Builder().url(endpoint).apply {
                if (secret.isNotEmpty()) header("Authorization", "Bearer $secret")
            }.build()
        }.getOrElse {
            mutableState.value = state.value.copy(error = context.getString(R.string.error_token_format))
            return false
        }
        if (runCatching { settings.save(url.trim(), secret) }.isFailure) {
            mutableState.value = state.value.copy(error = context.getString(R.string.error_token_save))
            return false
        }
        if (endpoint == connectedEndpoint) {
            closeSocket(graceful = true)
            retryDelay = 1_000
        } else {
            disconnect()
            connectedEndpoint = endpoint
        }
        attachments.useServer(endpoint)
        request = next
        maintainConnection()
        return true
    }

    fun updateLocale() {
        val locale = context.getString(R.string.ui_locale)
        if (welcomed && serverProtocol >= 22 && locale != sentLocale && send(ClientMessage.SetLocale(locale))) {
            sentLocale = locale
        }
    }

    fun disconnect() {
        request = null
        connectedEndpoint = null
        closeSocket(graceful = true)
        retryDelay = 1_000
        selectionToRestore = null
        updateActive = false
        recentNotices.clear()
        clearTranscripts()
        mutableState.value = RemoteState()
    }

    fun selectThread(id: String?) {
        selectionToRestore = id
        mutableState.value = state.value.copy(selection = id)
        requestSelectionTail()
        resumeOlderHistory()
    }

    fun requestThreadJob(session: String, job: String, older: Boolean) {
        val current = state.value
        if (current.selection != session || !current.canSend || replaying) return
        val timeline = current.timeline(session)
        val row = timeline.rows[job] as? TimelineRow.Job ?: return
        if (!row.info.detailsDeferred) return
        val before = if (older) row.nextDetail ?: return else null
        if (older && row.loadingDetails) return
        val request = if (older || row.detailRequest == null) ++nextHistoryRequest else null
        if (sendToWorkspace(ClientMessage.GetThreadJob(session, job, request, before))) {
            mutableState.value = current.withTimeline(session, timeline.updateJob(job) {
                it.copy(
                    detailRequest = request, details = if (older) it.details else null,
                    nextDetail = if (older) it.nextDetail else null, loadingDetails = request != null,
                )
            })
        }
    }

    fun requestOlderHistory(target: ConversationTarget) {
        val current = state.value
        if (serverProtocol < THREAD_HISTORY_PROTOCOL || current.workspace != target.workspace ||
            current.selection != target.session || !current.canSend || replaying
        ) return
        val history = histories[target.session]
        if (selectionScoped && history?.generation == null) {
            requestSelectionTail()
            return
        }
        if (history != null) requestHistoryPage(target.session, history)
    }

    private fun requestHistoryPage(session: String, history: TranscriptHistory) {
        val before = history.before ?: return
        if (history.page != null) return
        val request = ++nextHistoryRequest
        val generation = history.generation
        if (sendToWorkspace(ClientMessage.GetThreadHistory(session, request, before, generation))) {
            history.page = TranscriptHistory.PendingPage(request, before, generation)
            history.continueOlder = false
            val current = state.value
            mutableState.value = current.withTimeline(session, current.timeline(session).copy(historyEdge = history.edge))
        }
    }

    private fun resumeOlderHistory() {
        val current = state.value
        val session = current.selection ?: return
        val workspace = current.workspace ?: return
        val history = histories[session] ?: return
        if (history.pageInterrupted || history.continueOlder) {
            requestOlderHistory(ConversationTarget(workspace, session))
        }
    }

    private fun requestSelectionTail() {
        val current = state.value
        val session = current.selection ?: return
        if (!selectionScoped || replaying || !current.canSend || current.threads.none { it.id == session }) return
        val history = histories.getOrPut(session) { TranscriptHistory() }
        if (serverProtocol >= JOB_SUMMARY_PROTOCOL && !history.jobsRequested) {
            history.jobsRequested = sendToWorkspace(ClientMessage.GetThreadJobs(session))
        }
        if (history.generation != null || history.tail != null) return
        val request = ++nextHistoryRequest
        if (sendToWorkspace(ClientMessage.GetThreadTail(session, request))) {
            history.tail = TranscriptHistory.Pending(request)
        }
    }

    private fun clearTranscripts() {
        histories.clear()
        transcriptEpoch = null
        transcriptThrough = 0uL
        clearTranscriptReplay()
    }

    private fun clearTranscriptReplay() {
        transcriptReplay.clear()
        initialThreads = null
        initialActivity = null
    }

    fun switchWorkspace(id: String) {
        val current = state.value
        if (current.canSend && id != current.workspace && send(ClientMessage.SwitchWorkspace(id))) {
            mutableState.value = current.copy(changingWorkspace = true, error = null)
            attachments.connectionChanged()
        }
    }

    fun newThread() {
        val current = state.value
        if (!current.creatingThread && sendToWorkspace(ClientMessage.NewThread)) {
            mutableState.value = current.copy(creatingThread = true, error = null)
        }
    }

    fun prompt(session: String, text: String): Boolean {
        val current = state.value
        val workspace = current.workspace ?: return false
        if (current.threads.none { it.id == session }) return false
        val target = ConversationTarget(workspace, session)
        val paths = if (isComposerCommand(text)) emptyList() else attachments.readyPaths(target) ?: return false
        if (text.isBlank() && paths.isEmpty()) return false
        if (!sendToWorkspace(composerPrompt(session, text, paths))) return false
        val outbox = current.outboxes[target] ?: PromptOutbox(
            messages = emptyList(),
            seenUserItems = current.timelines[session]?.rows?.values.orEmpty()
                .mapNotNull { (it as? TimelineRow.Message)?.userItem }.toSet(),
        )
        val pending = PendingPrompt(
            id = nextPromptId++,
            text = text,
            sentAt = System.currentTimeMillis(),
            drainStartedAt = current.drain?.takeIf { it.holdingInput }?.startedAt,
            paths = paths,
        )
        mutableState.value = current.copy(
            outboxes = current.outboxes + (target to outbox.copy(messages = outbox.messages + pending)),
            error = null,
        )
        attachments.markSent(target, paths)
        return true
    }

    fun interrupt(session: String) {
        if (state.value.isTurnRunning(session)) sendToWorkspace(ClientMessage.Interrupt(session))
    }

    fun stopWorker(session: String, job: String) {
        val timeline = state.value.timeline(session)
        val worker = timeline.rows[job] as? TimelineRow.Job
        val summary = timeline.jobSummaries[job] ?: worker?.wireSummary
        if (summary?.status?.active ?: (worker != null && worker.ok == null)) {
            sendToWorkspace(ClientMessage.StopWorker(session, job))
        }
    }

    fun resolve(id: String, accept: Boolean) {
        val request = state.value.approvals[id] ?: return
        val message = if (request.isFolderAccess) {
            ClientMessage.DecideFolderAccess(id, if (accept) "one_hour" else null)
        } else {
            ClientMessage.Resolve(id, if (accept) "accept" else "decline")
        }
        resolve(id, message)
    }

    fun decideQueuedApprovals(
        target: ConversationTarget, selected: List<QueuedApproval>, accept: Boolean,
    ) {
        val current = state.value
        val dock = current.approvalDock
        if (!current.canSend || !approvalQueueReady || current.workspace != target.workspace ||
            current.selection != target.session || dock.decisions[target.session]?.isSending == true
        ) return
        val first = selected.firstOrNull() ?: return
        if (selected.any { it.workspace != target.workspace || it.thread != target.session } ||
            (selected.size > 1 && selected.any { !first.canBatchWith(it) })
        ) return

        val (ready, unavailable) = selected.distinctBy { it.id }.partition { it == dock.pending[it.id] }
        val requests = ready.associateBy { ++nextApprovalRequest }
        val progress = ApprovalDecisionProgress(
            requests = requests,
            failures = unavailable.map {
                ApprovalDecisionFailure(it, context.getString(R.string.approval_dock_unavailable))
            },
        )
        mutableState.value = current.copy(
            approvalDock = dock.copy(decisions = dock.decisions + (target.session to progress)),
        )
        val decision = if (accept) QueuedApprovalDecision.AllowOnce else QueuedApprovalDecision.Deny
        for ((request, approval) in requests) {
            if (!sendToWorkspace(ClientMessage.DecideApproval(request, approval.id, decision))) break
        }
    }

    fun dismissQueuedDecision(target: ConversationTarget) {
        val current = state.value
        val dock = current.approvalDock
        if (current.workspace != target.workspace || dock.decisions[target.session]?.isSending != false) return
        mutableState.value = current.copy(approvalDock = dock.copy(decisions = dock.decisions - target.session))
    }

    fun retryApprovalQueue(target: ConversationTarget) {
        val current = state.value
        if (!current.canSend || current.workspace != target.workspace || current.approvalDock.loadError == null) return
        requestApprovalQueue(target.workspace)
    }

    private fun requestApprovalQueue(workspace: String) {
        if (!approvalQueueSupported || approvalSnapshotRequest != null) return
        val request = ++nextApprovalRequest
        if (send(ClientMessage.InWorkspace(workspace, ClientMessage.ListPendingApprovals(request)))) {
            approvalSnapshotRequest = request
            val current = state.value
            mutableState.value = current.copy(approvalDock = current.approvalDock.copy(loadError = null, isLoading = true))
        }
    }

    private fun applyQueuedApproval(current: RemoteState, approval: QueuedApproval): RemoteState {
        if (approval.workspace != current.workspace) return current
        val dock = current.approvalDock
        val next = dock.update(approval)
        if (approval.state == ApprovalQueueState.Pending) {
            if (approval.id in resolved) return current
            if (approval.id !in dock.pending) {
                notifyThread(current, approval.workspace, approval.thread, null, R.string.approval_pending,
                    urgent = true, openApprovalDock = true)
            }
            return current.copy(approvalDock = next)
        }
        if (approval.id in next.pending) return current
        resolved.add(approval.id)
        return current.copy(
            approvalDock = next,
            approvals = current.approvals - approval.id,
            resolving = current.resolving - approval.id,
        ).withTimeline(approval.thread, current.timeline(approval.thread).recordApproval(ApprovalReceipt(
            approval.id, approval.job, approval.action.tool, approval.toolItem, approval.anchorItem,
            approval.state, approval.automatic, approval.updatedAt,
        )))
    }

    private fun finishQueuedDecision(current: RemoteState, event: ServerEvent.ApprovalDecisionResult): RemoteState {
        val dock = current.approvalDock
        val entry = dock.decisions.entries.firstOrNull { event.request in it.value.requests } ?: return current
        val request = entry.value.requests.getValue(event.request)
        val approval = event.result.value?.takeIf {
            it.id == request.id && it.workspace == request.workspace && it.thread == request.thread &&
                it.state != ApprovalQueueState.Pending
        }
        val error = if (approval != null) null else when (val message = event.result.error) {
            "approval request has already been decided or consumed" -> context.getString(R.string.approval_dock_conflict)
            null -> context.getString(R.string.approval_dock_unavailable)
            else -> message
        }
        val progress = entry.value.complete(event.request, error)
        val next = current.copy(approvalDock = dock.copy(decisions = dock.decisions + (entry.key to progress)))
        return if (approval == null) next else applyQueuedApproval(next, approval)
    }

    fun resolveNotification(approval: ApprovalNotice, accept: Boolean) {
        if (state.value.notificationApproval(approval) != null) resolve(approval.requestId, accept)
    }

    fun rememberShellApproval(id: String, prefix: String? = null) {
        val current = state.value
        val request = current.approvals[id] ?: return
        if (current.shellApprovalRules == null || request.shellDirectory == null) return
        if (prefix != null && prefix != request.shellPrefix) return
        resolve(id, ClientMessage.RememberShellApproval(id, prefix))
    }

    fun revokeShellApproval(id: String) {
        if (state.value.shellApprovalRules?.any { it.id == id } == true) {
            sendToWorkspace(ClientMessage.RevokeShellApproval(id))
        }
    }

    fun setWorkerApprovalPolicy(policy: WorkerApprovalPolicy) {
        val current = state.value.workerApprovalPolicy ?: return
        if (current != policy) sendToWorkspace(ClientMessage.SetWorkerApprovalPolicy(policy))
    }

    private fun resolve(id: String, message: ClientMessage) {
        val current = state.value
        if (id !in current.approvals || id in current.resolving || id in current.approvalDock.pending) return
        if (sendToWorkspace(message)) {
            mutableState.value = current.copy(resolving = current.resolving + id)
        }
    }

    private fun sendToWorkspace(message: ClientMessage): Boolean {
        val current = state.value
        val id = current.workspace ?: return false
        return current.canSend && send(ClientMessage.InWorkspace(id, message))
    }

    private fun send(message: ClientMessage): Boolean {
        if (socket?.send(Protocol.encode(message)) == true) return true
        if (socket != null) connectionLost(context.getString(R.string.error_send))
        return false
    }

    private fun maintainConnection() {
        val wanted = request
        if (wanted == null || (!visible && !serviceRunning)) {
            closeSocket(graceful = true)
            mutableState.value = state.value.copy(
                phase = ConnectionPhase.Disconnected, resolving = emptySet(),
            )
            return
        }
        if (socket != null) return
        handler.removeCallbacks(retry)
        welcomed = false
        replaying = true
        mutableState.value = state.value.copy(phase = ConnectionPhase.Connecting, error = null, updateRequired = false)
        socket = client.newWebSocket(wanted, listener)
        handler.postDelayed(helloTimeout, 30_000)
    }

    private fun closeSocket(graceful: Boolean = false) {
        handler.removeCallbacks(retry)
        handler.removeCallbacks(helloTimeout)
        serverProtocol = 0
        updateProtocol = null
        approvalQueueSupported = false
        approvalSnapshotRequest = null
        approvalQueueReady = false
        val current = state.value
        mutableState.value = current.copy(approvalDock = ApprovalDockState(
            decisions = current.approvalDock.decisions.mapValues { (_, progress) ->
                progress.interrupt(context.getString(R.string.approval_dock_interrupted))
            },
        ))
        sentTranscriptResume = false
        selectionScoped = false
        clearTranscriptReplay()
        for (history in histories.values) {
            history.jobsRequested = false
            history.tail = null
            history.interruptPage()
        }
        socket?.let {
            reconnectPromptLimit = nextPromptId
            if (!graceful || !it.close(1000, null)) it.cancel()
            attachments.connectionChanged()
        }
        socket = null
    }

    private fun connectionLost(
        message: String, fatal: Boolean = false, updateRequired: Boolean = false,
        protocol: UpdateProtocol? = null,
    ) {
        closeSocket()
        updateProtocol = protocol
        if (fatal) request = null
        val retrying = wantsConnection && (visible || serviceRunning)
        mutableState.value = state.value.copy(
            phase = if (retrying) ConnectionPhase.Retrying else ConnectionPhase.Disconnected,
            changingWorkspace = false, creatingThread = false, resolving = emptySet(),
            error = message,
            updateRequired = updateRequired,
        )
        if (retrying) {
            handler.postDelayed(retry, retryDelay)
            retryDelay = (retryDelay * 2).coerceAtMost(30_000)
        }
    }

    private val listener = object : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            handler.post {
                if (socket === webSocket) {
                    val locale = context.getString(R.string.ui_locale)
                    val workspace = state.value.workspace
                    val epoch = transcriptEpoch.takeIf { workspace != null }
                    sentTranscriptResume = epoch != null
                    val hello = ClientMessage.Hello(
                        workspace = workspace, locale = locale, epoch = epoch,
                        resumeFrom = transcriptThrough.takeIf { epoch != null },
                        cachedSessions = epoch?.let {
                            histories.mapNotNull { (session, history) ->
                                history.generation?.let { generation -> CachedSession(session, generation) }
                            }
                        },
                    )
                    if (send(hello)) {
                        sentLocale = locale
                    }
                }
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            val frame = runCatching { Protocol.decode(text) }.getOrNull()
            handler.post {
                if (socket !== webSocket) return@post
                if (frame == null) connectionLost(context.getString(R.string.error_decode), fatal = true)
                else receive(frame)
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            handler.post {
                if (socket !== webSocket) return@post
                when (val code = response?.code) {
                    401, 403 -> connectionLost(context.getString(R.string.error_connection_denied), fatal = true)
                    null -> connectionLost(context.getString(R.string.error_connection))
                    else -> connectionLost(context.getString(R.string.error_websocket, code), fatal = true)
                }
            }
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            webSocket.close(1000, null)
            handler.post {
                if (socket === webSocket) {
                    val startupError = state.value.error.takeUnless { welcomed }
                    connectionLost(startupError ?: context.getString(R.string.error_connection_lost), fatal = startupError != null)
                }
            }
        }
    }

    private fun receive(frame: ServerFrame) {
        val event = frame.event
        if (event is ServerEvent.Welcome) {
            handler.removeCallbacks(helloTimeout)
            val agreed = minOf(PROTOCOL_VERSION, event.protocol)
            val required = event.minimumProtocol ?: agreed
            val updateRange = UpdateProtocol(event.minimumProtocol ?: 1, maxOf(event.protocol, required))
            if (agreed < MIN_PROTOCOL_VERSION || required > agreed) {
                connectionLost(
                    context.getString(R.string.error_protocol_version, PROTOCOL_VERSION, maxOf(agreed, required)),
                    fatal = true,
                    updateRequired = required > PROTOCOL_VERSION,
                    protocol = updateRange,
                )
            } else {
                selectionScoped = agreed >= THREAD_HISTORY_PROTOCOL && event.selectionScoped
                if (sentTranscriptResume && !selectionScoped) {
                    // A legacy server can interpret resume_from as an eager backlog cursor.
                    closeSocket()
                    clearTranscripts()
                    maintainConnection()
                    return
                }
                serverProtocol = agreed
                updateProtocol = updateRange
                approvalQueueSupported = event.approvalQueue
                welcomed = true
                receivedDrainStatus = false
                updateLocale()
                // Keep the timeout active until the server sends its initial thread snapshot.
                handler.postDelayed(helloTimeout, 60_000)
            }
            return
        }
        if (!welcomed) {
            if (event is ServerEvent.Error) {
                val minimum = event.minimumProtocol ?: 0
                if (minimum > PROTOCOL_VERSION) {
                    connectionLost(event.message, fatal = true, updateRequired = true,
                        protocol = UpdateProtocol(minimum, minimum))
                } else mutableState.value = state.value.copy(error = event.message)
            }
            return
        }
        val current = state.value
        if (event !is ServerEvent.Notify) {
            if (frame.workspace != null && frame.workspace != current.workspace) return
            if (selectionScoped && !replaying && frame.workspace != null && frame.seq != 0uL) {
                if (frame.seq <= transcriptThrough) return
                transcriptThrough = frame.seq
            }
        }
        mutableState.value = when (event) {
            is ServerEvent.AttachmentProgress -> {
                attachments.progress(event.id, event.offset)
                return
            }
            is ServerEvent.AttachmentUploaded -> {
                attachments.uploaded(event.id, event.path.takeIf { event.error == null })
                return
            }
            is ServerEvent.WorkspaceSelected -> {
                val changed = event.id != current.workspace
                if (changed || event.replay) {
                    approvalSnapshotRequest = null
                    approvalQueueReady = false
                }
                if (changed) selectionToRestore = null
                if (changed || event.replay) {
                    clearTranscripts()
                    replaying = true
                    resolved.clear()
                    RemoteState(
                        phase = ConnectionPhase.Connecting,
                        workspace = event.id,
                        workspaces = current.workspaces,
                        threads = if (changed) emptyList() else current.threads,
                        selection = current.selection.takeUnless { changed },
                        outboxes = current.outboxes,
                        drain = current.drain.takeIf { receivedDrainStatus },
                        heldConversations = current.heldConversations.takeIf { receivedDrainStatus }.orEmpty(),
                    )
                } else if (selectionScoped && replaying) {
                    resolved.clear()
                    current.copy(
                        approvals = emptyMap(), resolving = emptySet(), waiting = ChatWaiting(),
                        changingWorkspace = false, error = null,
                    )
                } else current.copy(changingWorkspace = false, error = null)
            }
            is ServerEvent.ApprovalQueue -> {
                val workspace = current.workspace ?: return
                if (!approvalQueueSupported || event.request != approvalSnapshotRequest) return
                approvalSnapshotRequest = null
                val approvals = event.result.value
                approvalQueueReady = approvals != null
                val dock = current.approvalDock
                current.copy(approvalDock = if (approvals == null) {
                    dock.copy(
                        loadError = event.result.error ?: context.getString(R.string.approval_dock_unavailable),
                        isLoading = false,
                    )
                } else dock.replace(workspace, approvals.filterNot { it.id in resolved }))
            }
            is ServerEvent.ApprovalQueueChanged -> {
                if (!approvalQueueReady) return
                applyQueuedApproval(current, event.approval)
            }
            is ServerEvent.ApprovalDecisionResult -> finishQueuedDecision(current, event)
            is ServerEvent.Workspaces -> current.copy(workspaces = event.workspaces)
            is ServerEvent.WorkerApprovalPolicyChanged -> current.copy(
                workerApprovalPolicy = event.policy, shellApprovalRules = event.shellRules,
            )
            is ServerEvent.WaitingChanged -> current.copy(waiting = event.waiting)
            is ServerEvent.DrainChanged -> {
                receivedDrainStatus = true
                current.copy(
                    drain = event.status,
                    heldConversations = current.heldConversations.takeIf {
                        current.drain?.startedAt == event.status.startedAt && event.status.holdingInput
                    }.orEmpty(),
                )
            }
            ServerEvent.PartnerDefaults -> current
            is ServerEvent.Notify -> {
                val workspace = frame.workspace ?: return
                // The live approval event for the selected workspace identifies the
                // exact request. Other workspaces only have the navigation notice.
                if (event.kind == "approval" && current.workspace == workspace) return
                val (message, urgent) = when (event.kind) {
                    "turn_finished" -> R.string.notice_reply_finished to false
                    "turn_failed" -> R.string.notice_reply_failed to false
                    "approval" -> R.string.approval_pending to true
                    "question" -> R.string.notice_question to true
                    "goal_blocked" -> R.string.notice_goal_blocked to true
                    "worker_finished" -> R.string.notice_worker_finished to false
                    "worker_failed" -> R.string.notice_worker_failed to false
                    "worker_stalled" -> R.string.notice_worker_stalled to false
                    else -> {
                        Log.d(LOG_TAG, "Ignoring notification kind: ${event.kind}")
                        return
                    }
                }
                if (event.kind.startsWith("worker_") && !settings.workerNotices) return
                notifyThread(current, workspace, event.session, event.title, message, urgent)
                current
            }
            is ServerEvent.UpdateChanged -> {
                val status = event.status
                val wasActive = updateActive
                updateActive = status.active
                if (wasActive && !status.active && !status.cancelled) {
                    val message = when {
                        status.error != null -> R.string.notice_update_failed
                        status.steps.any { it.kind == "swap" && it.state == "passed" } -> R.string.notice_update_finished
                        else -> null
                    }
                    if (message != null && !visible) notify(null, message, "fuzukue", false)
                }
                current
            }
            is ServerEvent.Threads -> {
                if (selectionScoped && replaying) {
                    initialThreads = event
                    return
                }
                updateThreads(current, event)
            }
            is ServerEvent.ChatActivity -> {
                if (selectionScoped && replaying) {
                    initialActivity = event
                    return
                }
                applyActivity(current, event)
            }
            is ServerEvent.ThreadSync -> {
                if (!selectionScoped || !replaying || frame.workspace != current.workspace || frame.workspace == null) return
                finishTranscriptSync(current, event)
            }
            is ServerEvent.ThreadHistoryStart -> {
                if (selectionScoped || serverProtocol < THREAD_HISTORY_PROTOCOL) return
                val history = histories.getOrPut(event.session) { TranscriptHistory() }
                history.startLegacy(event)
                current.withTimeline(event.session, current.timeline(event.session).copy(historyEdge = history.edge))
            }
            is ServerEvent.ThreadTail -> {
                if (!selectionScoped || replaying || frame.workspace != current.workspace || frame.workspace == null) return
                val history = histories[event.session] ?: return
                val pending = history.tail?.takeIf { it.request == event.request } ?: return
                val events = savedTranscriptEvents(event.session, event.events)
                val loaded = history.applyTail(event, current.timeline(event.session), pending, events)
                // A prompt echoed across the disconnect may be present only in the new tail.
                events.fold(current.withTimeline(event.session, loaded.copy(historyEdge = history.edge))) { next, echo ->
                    acknowledgePrompt(next, echo, reconnectPromptLimit)
                }
            }
            is ServerEvent.ThreadJobDetails -> {
                if (!selectionScoped || replaying || frame.workspace == null || frame.workspace != current.workspace) return
                val details = savedTranscriptEvents(event.session, event.events)
                val timeline = details.filterIsInstance<ServerEvent.ApprovalRecorded>()
                    .fold(current.timeline(event.session)) { state, record -> state.recordApproval(record.receipt) }
                current.withTimeline(event.session, timeline.updateJob(event.job) { job ->
                    if (job.detailRequest != event.request) job else job.copy(
                        details = details.mapNotNull { nested ->
                            when (nested) {
                                is ServerEvent.TextDelta -> nested.text
                                is ServerEvent.UserPrompt -> nested.text
                                is ServerEvent.ToolCall -> if (nested.item == event.job) null else "${nested.name}\n${nested.input}"
                                is ServerEvent.ToolResult -> if (nested.item == event.job) {
                                    runCatching { Protocol.json.decodeFromString<JobReport>(nested.output).text }.getOrNull()
                                } else nested.output
                                is ServerEvent.DelegateProgress -> nested.summary
                                else -> null
                            }?.takeIf { it.isNotBlank() }
                        } + (if (event.before != null) job.details.orEmpty() else emptyList()),
                        nextDetail = event.nextBefore, loadingDetails = false,
                    )
                })
            }
            is ServerEvent.ThreadJobs -> {
                if (!selectionScoped || frame.workspace == null || frame.workspace != current.workspace ||
                    !histories.containsKey(event.session)) return
                val timeline = current.timeline(event.session)
                current.withTimeline(event.session, timeline.copy(
                    jobSummaries = timeline.jobSummaries.snapshot(event.through, event.first, event.jobs),
                ))
            }
            is ServerEvent.ThreadJobUpdated -> {
                if (!selectionScoped || frame.workspace == null || frame.workspace != current.workspace ||
                    !histories.containsKey(event.session)) return
                val timeline = current.timeline(event.session)
                current.withTimeline(event.session, timeline.copy(
                    jobSummaries = timeline.jobSummaries.update(event.through, event.summary),
                ))
            }
            is ServerEvent.ThreadHistoryPage -> {
                if (replaying || (selectionScoped && frame.workspace == null)) return
                val history = histories[event.session] ?: return
                val before = current.timeline(event.session)
                val merged = history.applyPage(event, before) ?: return
                // Hidden rows and updates to a sparse worker can leave the same visible extent.
                history.continueOlder = history.before != null &&
                    merged.visibleRows.keys.all(before.visibleRows.keys::contains)
                current.withTimeline(event.session, merged.copy(historyEdge = history.edge))
            }
            is ServerEvent.ThreadHistoryFailed -> {
                if (replaying || (selectionScoped && frame.workspace == null)) return
                val history = histories[event.session] ?: return
                if (history.tail?.request == event.request) {
                    history.tail = null
                    return
                }
                if (history.page?.request != event.request) return
                when (event.reason) {
                    "stale", "not_found" -> invalidateHistory(current, event.session)
                    else -> current.withTimeline(
                        event.session,
                        history.finishPage(current.timeline(event.session), endOfHistory = event.reason == null)
                            .copy(historyEdge = if (event.reason == null) HistoryEdge.Unknown else history.edge),
                    )
                }
            }
            is ServerEvent.ThreadHistoryInvalidated -> {
                if (!selectionScoped || frame.workspace == null) return
                invalidateHistory(current, event.session)
            }
            is ServerEvent.ThreadRestored -> {
                if (selectionScoped) return
                histories.remove(event.session)
                val restored = savedTranscriptEvents(event.session, event.events).fold(
                    current.timeline(event.session).clearHistory(), Timeline::reduceSaved,
                )
                current.withTimeline(event.session, restored)
            }
            is ServerEvent.ThreadCreated -> {
                selectionToRestore = event.id
                current.copy(selection = event.id, creatingThread = false)
            }
            is ServerEvent.ApprovalRequested -> {
                if (event.requestId in current.approvals || event.requestId in resolved) return
                val next = current.copy(approvals = current.approvals + (event.requestId to event))
                mutableState.value = next
                current.workspace?.takeUnless { selectionScoped && replaying }?.let { workspace ->
                    val target = ConversationTarget(workspace, event.session)
                    val approval = ApprovalNotice(target, event.requestId).takeIf { next.notificationApproval(it) != null }
                    notifyThread(next, workspace, event.session, null, R.string.approval_pending, true, approval)
                }
                return
            }
            is ServerEvent.ApprovalResolved -> {
                resolved.add(event.requestId)
                current.copy(
                    approvals = current.approvals - event.requestId,
                    resolving = current.resolving - event.requestId,
                    approvalDock = current.approvalDock.copy(pending = current.approvalDock.pending - event.requestId),
                )
            }
            is ServerEvent.InThread -> {
                val before = current.timeline(event.session)
                val after = if (selectionScoped && event.isSavedTranscriptEvent) {
                    val update = TranscriptUpdate(frame.seq, event)
                    if (replaying) {
                        transcriptReplay.add(update)
                        return
                    }
                    val history = histories[event.session]
                    history?.tail?.updates?.add(update)
                    if (history?.generation != null) history.applyUpdate(before, update)
                    else before.observeActivity(event)
                } else before.reduce(event, replaying = replaying)
                // A live turn can start before the next waiting snapshot arrives.
                val waiting = if (!replaying && event is ServerEvent.TurnActivity && event.running &&
                    current.waiting.reason(event.session) != null
                ) {
                    current.waiting.copy(threads = current.waiting.threads + (event.session to null))
                } else current.waiting
                acknowledgePrompt(current, event).copy(
                    timelines = if (before == after) current.timelines else current.timelines + (event.session to after),
                    waiting = waiting,
                )
            }
            is ServerEvent.Error -> current.copy(
                error = event.message, changingWorkspace = false, creatingThread = false,
                resolving = emptySet(),
            )
            is ServerEvent.Unknown -> {
                Log.d(LOG_TAG, "Ignoring server event: ${event.type ?: "missing or invalid type"}")
                return
            }
            is ServerEvent.Welcome -> current
        }
        when (event) {
            is ServerEvent.ThreadSync -> {
                requestSelectionTail()
                resumeOlderHistory()
            }
            is ServerEvent.WorkspaceSelected -> {
                if (!approvalQueueReady && approvalSnapshotRequest == null) requestApprovalQueue(event.id)
                requestSelectionTail()
            }
            is ServerEvent.Threads, is ServerEvent.ThreadCreated, is ServerEvent.ThreadHistoryInvalidated,
            is ServerEvent.Error -> requestSelectionTail()
            is ServerEvent.ThreadHistoryFailed -> if (event.reason == "stale") requestSelectionTail()
            is ServerEvent.ThreadHistoryPage -> resumeOlderHistory()
            else -> Unit
        }
    }

    private fun updateThreads(current: RemoteState, event: ServerEvent.Threads): RemoteState {
        val threads = event.threads.filterNot { it.archived }
            .sortedWith(compareByDescending<ThreadInfo> { it.pinned }.thenByDescending { it.updated })
        val listed = threads.mapTo(mutableSetOf()) { it.id }
        val removed = current.threads.map { it.id }.filterNot { it in listed }.toSet()
        histories.keys.removeAll(removed)
        val selection = (current.selection ?: selectionToRestore)?.takeIf { it in listed }
        if (replaying) {
            replaying = false
            handler.removeCallbacks(helloTimeout)
            retryDelay = 1_000
        }
        return current.copy(
            threads = threads, selection = selection, phase = ConnectionPhase.Connected,
            timelines = current.timelines - removed,
        )
    }

    private fun finishTranscriptSync(current: RemoteState, event: ServerEvent.ThreadSync): RemoteState {
        val reset = if (transcriptEpoch != event.epoch || transcriptThrough > event.through) histories.keys.toSet()
            else event.resetSessions
        var next = current
        for (session in reset) next = invalidateHistory(next, session)
        for (update in transcriptReplay) {
            if (update.seq <= transcriptThrough || update.seq > event.through) continue
            val session = update.event.session
            val history = histories[session]?.takeIf { it.generation != null } ?: continue
            next = acknowledgePrompt(next, update.event).withTimeline(
                session, history.applyUpdate(next.timeline(session), update, savedOnly = true),
            )
        }
        next = next.copy(timelines = next.timelines.mapValues { (_, timeline) -> timeline.copy(pauseReason = null) })
        initialActivity?.let { next = applyActivity(next, it) }
        next = updateThreads(next, initialThreads ?: ServerEvent.Threads(next.threads))
        transcriptEpoch = event.epoch
        transcriptThrough = event.through
        clearTranscriptReplay()
        return next
    }

    private fun applyActivity(current: RemoteState, event: ServerEvent.ChatActivity): RemoteState {
        val timelines = (current.timelines.keys + event.turns).associateWith { session ->
            current.timeline(session).withActivity(session in event.turns)
        }
        return current.copy(timelines = timelines)
    }

    private fun invalidateHistory(current: RemoteState, session: String): RemoteState {
        histories.remove(session)
        return current.withTimeline(session, current.timeline(session).clearHistory().copy(jobSummaries = JobSummaries()))
    }

    private fun acknowledgePrompt(
        current: RemoteState, event: ServerEvent.InThread, promptLimit: Long = Long.MAX_VALUE,
    ): RemoteState {
        val workspace = current.workspace ?: return current
        val target = ConversationTarget(workspace, event.session)
        val outbox = current.outboxes[target] ?: return current
        if (outbox.messages.none { it.id < promptLimit }) return current
        val (item, text) = when (event) {
            is ServerEvent.TextDelta -> if (event.actor.kind == "user") event.item to event.text else return current
            is ServerEvent.UserPrompt -> if (!event.toWorker) event.item to event.text else return current
            else -> return current
        }
        // The server echoes input with its own item ID. During replay, skip echoes already
        // seen before sending or used to acknowledge another pending message.
        if (item in outbox.seenUserItems) return current
        val paths = if (event is ServerEvent.UserPrompt) event.paths else emptyList()
        val acknowledged = outbox.messages.firstOrNull {
            it.id < promptLimit && it.paths == paths &&
                (it.text == text || (it.text.startsWith("//") && it.text.drop(1) == text))
        }
        val remaining = if (acknowledged == null) outbox.messages else outbox.messages - acknowledged
        val updated = outbox.copy(messages = remaining, seenUserItems = outbox.seenUserItems + item)
        // DrainStatus is global. The echo confirms acceptance, and the drain observed at
        // submission tells us whether to show the hold note.
        val drain = current.drain?.takeIf { it.holdingInput }
        val held = drain != null && acknowledged?.drainStartedAt == drain.startedAt
        return current.copy(
            outboxes = if (remaining.isEmpty()) current.outboxes - target else current.outboxes + (target to updated),
            heldConversations = if (held) current.heldConversations + target else current.heldConversations,
        )
    }

    private fun notifyThread(
        current: RemoteState, workspace: String, session: String, title: String?, message: Int, urgent: Boolean,
        approval: ApprovalNotice? = null, openApprovalDock: Boolean = false,
    ) {
        val sameWorkspace = current.workspace == workspace
        if (replaying || (visible && sameWorkspace && current.selection == session)) return
        val thread = current.threads.find { it.id == session }.takeIf { sameWorkspace }
        val threadTitle = when {
            thread?.untitled == true -> context.getString(R.string.thread_untitled)
            else -> thread?.title ?: title ?: context.getString(R.string.thread)
        }
        notify(ConversationTarget(workspace, session), message, threadTitle, urgent, approval, openApprovalDock)
    }

    private fun notify(
        target: ConversationTarget?, message: Int, threadTitle: String, urgent: Boolean,
        approval: ApprovalNotice? = null, openApprovalDock: Boolean = false,
    ) {
        if (!serviceRunning) return
        val now = SystemClock.elapsedRealtime()
        recentNotices.entries.removeAll { now - it.value.first >= 20_000L }
        if (recentNotices[target]?.let { it.second || !urgent } == true) return
        if (mutableNotices.tryEmit(EventNotice(context.getString(message), threadTitle, target, approval, openApprovalDock))) {
            recentNotices[target] = now to urgent
        }
    }
}

internal fun socketUrl(value: String): String? {
    val input = value.trim()
    val http = when {
        input.startsWith("ws://", ignoreCase = true) -> "http://" + input.substring(5)
        input.startsWith("wss://", ignoreCase = true) -> "https://" + input.substring(6)
        else -> input
    }
    val url = http.toHttpUrlOrNull() ?: return null
    val authority = http.substringAfter("://").substringBefore('/')
    if ('@' in authority || '\\' in http || url.encodedQuery != null || url.encodedFragment != null) return null
    return if (url.encodedPath == "/") url.newBuilder().encodedPath("/ws").build().toString() else url.toString()
}
