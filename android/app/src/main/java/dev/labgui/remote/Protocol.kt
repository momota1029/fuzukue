package dev.labgui.remote

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import kotlinx.serialization.json.decodeFromJsonElement

internal const val JOB_SUMMARY_PROTOCOL = 74
const val PROTOCOL_VERSION = JOB_SUMMARY_PROTOCOL
internal const val MIN_PROTOCOL_VERSION = 18
internal const val THREAD_HISTORY_PROTOCOL = 62
internal const val FOLDER_ACCESS_REQUEST = "folder-access-"
private const val COMPOSER_NOTICE_ITEM = "composer-status"

@Serializable
sealed interface ClientMessage {
    @Serializable @SerialName("hello")
    data class Hello(
        val protocol: Int = PROTOCOL_VERSION,
        val workspace: String? = null,
        val locale: String? = null,
        val platform: String = "android",
        @SerialName("thread_history") val threadHistory: Boolean = true,
        @SerialName("selection_scoped_transcripts") val selectionScoped: Boolean = true,
        val epoch: String? = null,
        @SerialName("resume_from") val resumeFrom: ULong? = null,
        @SerialName("cached_sessions") val cachedSessions: List<CachedSession>? = null,
    ) : ClientMessage

    @Serializable @SerialName("get_thread_tail")
    data class GetThreadTail(val session: String, val request: ULong) : ClientMessage

    @Serializable @SerialName("get_thread_jobs")
    data class GetThreadJobs(val session: String) : ClientMessage

    @Serializable @SerialName("get_thread_job")
    data class GetThreadJob(
        val session: String, val job: String, val request: ULong? = null, val before: ULong? = null,
    ) : ClientMessage

    @Serializable @SerialName("get_thread_history")
    data class GetThreadHistory(
        val session: String, val request: ULong, val before: ULong, val generation: ULong? = null,
    ) : ClientMessage

    @Serializable @SerialName("set_locale")
    data class SetLocale(val locale: String) : ClientMessage

    @Serializable @SerialName("switch_workspace")
    data class SwitchWorkspace(val id: String) : ClientMessage

    @Serializable @SerialName("in_workspace")
    data class InWorkspace(val id: String, val request: ClientMessage) : ClientMessage

    @Serializable @SerialName("new_thread")
    data object NewThread : ClientMessage

    @Serializable @SerialName("prompt")
    data class Prompt(val session: String, val text: String) : ClientMessage

    @Serializable @SerialName("upload_attachment")
    data class UploadAttachment(
        val id: Long, val name: String, val size: Long, val offset: Long, val data: String, val session: String,
    ) : ClientMessage

    @Serializable @SerialName("cancel_attachment")
    data class CancelAttachment(val id: Long) : ClientMessage

    @Serializable @SerialName("prompt_with_attachments")
    data class PromptWithAttachments(val session: String, val text: String, val paths: List<String>) : ClientMessage

    @Serializable @SerialName("interrupt")
    data class Interrupt(val session: String) : ClientMessage

    @Serializable @SerialName("stop_worker")
    data class StopWorker(val session: String, val job: String) : ClientMessage

    @Serializable @SerialName("decide_folder_access")
    data class DecideFolderAccess(
        @SerialName("request_id") val requestId: String,
        val expiry: String?,
    ) : ClientMessage

    @Serializable @SerialName("list_pending_approvals")
    data class ListPendingApprovals(val request: ULong) : ClientMessage

    @Serializable @SerialName("decide_approval")
    data class DecideApproval(
        val request: ULong,
        @SerialName("approval_id") val approvalId: String,
        val decision: QueuedApprovalDecision,
    ) : ClientMessage

    @Serializable @SerialName("resolve")
    data class Resolve(@SerialName("request_id") val requestId: String, val decision: String) : ClientMessage

    @Serializable @SerialName("set_worker_approval_policy")
    data class SetWorkerApprovalPolicy(val policy: WorkerApprovalPolicy) : ClientMessage

    @Serializable @SerialName("remember_shell_approval")
    data class RememberShellApproval(
        @SerialName("request_id") val requestId: String,
        val prefix: String? = null,
    ) : ClientMessage

    @Serializable @SerialName("revoke_shell_approval")
    data class RevokeShellApproval(val id: String) : ClientMessage
}

@Serializable
data class CachedSession(val session: String, val generation: ULong)

@Serializable
data class Workspace(val id: String, val title: String, val path: String)

@Serializable
data class ThreadInfo(
    val id: String,
    val title: String,
    val updated: Long,
    val pinned: Boolean = false,
    val archived: Boolean = false,
    val untitled: Boolean = false,
)

@Serializable
data class Actor(val kind: String, @SerialName("parent_tool_use") val parentToolUse: String? = null) {
    val isPartner: Boolean get() = kind == "claude" || (kind == "codex" && parentToolUse == null)
}

@Serializable
data class JobInfo(
    val model: String,
    val agent: String? = null,
    @SerialName("started_at_ms") val startedAt: Long? = null,
    val title: String? = null,
    val label: String? = null,
    @SerialName("card_id") val cardId: String? = null,
    @SerialName("details_deferred") val detailsDeferred: Boolean = false,
    @SerialName("tool_count") val toolCount: Int = 0,
    @SerialName("queued_at_ms") val queuedAt: Long? = null,
    val selection: JobSelection? = null,
    val routing: JobRouting? = null,
    @SerialName("dismissed_from_panel") val dismissedFromPanel: Boolean = false,
) {
    val displayTitle: String get() = label ?: title?.takeIf { it.isNotBlank() && it != cardId } ?: agent ?: model
}

@Serializable
data class JobSelection(val backend: String, val model: String? = null, val effort: String? = null)

@Serializable
data class JobRouting(val tier: String)

@Serializable
enum class JobStatus {
    @SerialName("queued") Queued,
    @SerialName("running") Running,
    @SerialName("succeeded") Succeeded,
    @SerialName("failed") Failed,
    @SerialName("interrupted") Interrupted;

    val active: Boolean get() = this == Queued || this == Running
}

@Serializable
data class JobSummary(
    val id: String, val position: Int, val job: JobInfo, val status: JobStatus, val tools: Int,
    @SerialName("elapsed_ms") val elapsed: Long? = null,
    @SerialName("last_activity") val lastActivity: String? = null,
    @SerialName("last_tool") val lastTool: String? = null,
)

@Serializable
data class JobNameChange(val label: String)

@Serializable
data class JobReport(@SerialName("elapsed_ms") val elapsed: Long, val text: String)

@Serializable
data class Plan(
    val status: String,
    val next: List<PlanStep>,
    val question: String? = null,
    val goal: String = "",
)

@Serializable
data class PlanStep(val task: String, val blocker: PlanBlocker? = null)

@Serializable
data class PlanBlocker(val reason: String)

@Serializable
data class GoalProgress(val state: String)

@Serializable
data class WaitReason(val kind: String)

@Serializable
data class ChatWaiting(
    val workspace: WaitReason? = null,
    val threads: Map<String, WaitReason?> = emptyMap(),
) {
    fun reason(session: String): WaitReason? = threads.getOrDefault(session, workspace)
}

@Serializable
data class DrainStatus(
    @SerialName("started_at") val startedAt: Long,
    val reason: String,
    val progress: String,
) {
    val holdingInput: Boolean get() = when (progress) {
        "draining", "forcing", "ready", "timed_out" -> true
        else -> false
    }
}

@Serializable
enum class WorkerApprovalPolicy {
    @SerialName("ask") Ask,
    @SerialName("advisor") Advisor,
    @SerialName("auto") Auto,
}

@Serializable
enum class ApprovalRisk {
    @SerialName("low") Low,
    @SerialName("mid") Mid,
    @SerialName("high") High,
}

@Serializable
enum class ApprovalQueueState {
    @SerialName("pending") Pending,
    @SerialName("granted") Granted,
    @SerialName("denied") Denied,
    @SerialName("withdrawn") Withdrawn,
    @SerialName("expired") Expired,
}

@Serializable
data class QueuedApprovalAction(val tool: String, val args: JsonElement, val scope: JsonElement)

@Serializable
data class QueuedApproval(
    @SerialName("approval_id") val id: String,
    val workspace: String,
    val thread: String,
    val job: String,
    val requester: String,
    val risk: ApprovalRisk,
    val unattended: String,
    val reason: String,
    @SerialName("created_at") val createdAt: Long,
    @SerialName("expires_at") val expiresAt: Long,
    @SerialName("updated_at") val updatedAt: Long,
    val state: ApprovalQueueState,
    val action: QueuedApprovalAction,
    val automatic: Boolean = false,
    @SerialName("tool_item") val toolItem: String? = null,
    @SerialName("anchor_item") val anchorItem: String? = null,
)

@Serializable
data class ApprovalReceipt(
    val id: String,
    val job: String,
    val tool: String,
    @SerialName("tool_item") val toolItem: String? = null,
    @SerialName("anchor_item") val anchorItem: String? = null,
    val state: ApprovalQueueState,
    val automatic: Boolean,
    @SerialName("updated_at") val updatedAt: Long,
) {
    val workerJob: String? get() = job.takeUnless { it == "front_desk" }
    val anchor: String? get() = when (state) {
        ApprovalQueueState.Granted -> toolItem ?: anchorItem
        ApprovalQueueState.Denied -> anchorItem ?: workerJob
        else -> null
    }
}

@Serializable
data class QueuedApprovalDecision private constructor(val kind: String, val scope: JsonObject? = null) {
    companion object {
        val AllowOnce = QueuedApprovalDecision("grant", buildJsonObject { put("kind", "once") })
        val Deny = QueuedApprovalDecision("deny")
    }
}

@Serializable
data class ApprovalReply<T>(
    @SerialName("Ok") val value: T? = null,
    @SerialName("Err") val error: String? = null,
)

@Serializable
data class ShellApprovalRule(val id: String, val directory: String, val command: String, val prefix: String? = null)

sealed interface ServerEvent {
    data class Unknown(val type: String?) : ServerEvent
    @Serializable data class Welcome(
        val protocol: Int,
        @SerialName("minimum_protocol") val minimumProtocol: Int? = null,
        @SerialName("selection_scoped_transcripts") val selectionScoped: Boolean = false,
        @SerialName("approval_queue") val approvalQueue: Boolean = false,
    ) : ServerEvent
    @Serializable data class ApprovalQueue(
        val request: ULong,
        val result: ApprovalReply<List<QueuedApproval>>,
    ) : ServerEvent
    @Serializable data class ApprovalQueueChanged(val approval: QueuedApproval) : ServerEvent
    @Serializable data class ApprovalDecisionResult(
        val request: ULong,
        val result: ApprovalReply<QueuedApproval>,
    ) : ServerEvent
    @Serializable data class WorkspaceSelected(val id: String, val replay: Boolean) : ServerEvent
    @Serializable data class Workspaces(val workspaces: List<Workspace>) : ServerEvent
    @Serializable data class Threads(val threads: List<ThreadInfo>) : ServerEvent
    @Serializable data class ThreadCreated(val id: String) : ServerEvent
    @Serializable data class Notify(val session: String, val kind: String, val title: String? = null) : ServerEvent
    @Serializable data class WorkerApprovalPolicyChanged(
        val policy: WorkerApprovalPolicy,
        @SerialName("shell_rules") val shellRules: List<ShellApprovalRule>? = null,
    ) : ServerEvent
    @Serializable data class UpdateChanged(val status: ServerUpdateStatus) : ServerEvent
    @Serializable data class Error(
        val message: String,
        @SerialName("minimum_protocol") val minimumProtocol: Int? = null,
    ) : ServerEvent
    @Serializable data class WaitingChanged(val waiting: ChatWaiting) : ServerEvent
    @Serializable data class DrainChanged(val status: DrainStatus) : ServerEvent
    @Serializable data class AttachmentProgress(val id: Long, val offset: Long) : ServerEvent
    @Serializable data class AttachmentUploaded(
        val id: Long, val path: String? = null, val error: String? = null,
    ) : ServerEvent
    @Serializable data class ChatActivity(val turns: Set<String>) : ServerEvent
    @Serializable data class ThreadSync(
        val epoch: String, val through: ULong, @SerialName("reset_sessions") val resetSessions: Set<String>,
    ) : ServerEvent
    @Serializable data class ThreadHistoryStart(
        val session: String, val before: ULong, val workers: List<String>,
    ) : ServerEvent
    @Serializable data class ThreadTail(
        val session: String, val request: ULong, val generation: ULong, val through: ULong,
        val events: List<JsonObject>, val workers: List<String>, @SerialName("next_before") val nextBefore: ULong? = null,
    ) : ServerEvent
    @Serializable data class ThreadHistoryPage(
        val session: String, val request: ULong, val events: List<JsonObject>,
        @SerialName("next_before") val nextBefore: ULong? = null, val generation: ULong? = null, val through: ULong? = null,
    ) : ServerEvent
    @Serializable data class ThreadJobDetails(
        val session: String, val job: String, val request: ULong, val events: List<JsonObject>,
        val before: ULong? = null, @SerialName("next_before") val nextBefore: ULong? = null,
    ) : ServerEvent
    @Serializable data class ThreadJobActivity(
        override val session: String, val job: String, val tools: Int,
    ) : InThread
    @Serializable data class ThreadJobs(
        val session: String, val through: ULong, val first: Boolean, val jobs: List<JobSummary>,
    ) : ServerEvent
    @Serializable data class ThreadJobUpdated(
        val session: String, val through: ULong, val summary: JobSummary,
    ) : ServerEvent
    @Serializable data class ThreadHistoryFailed(val session: String, val request: ULong, val reason: String? = null) : ServerEvent
    @Serializable data class ThreadHistoryInvalidated(val session: String) : ServerEvent
    @Serializable data class ThreadRestored(val session: String, val events: List<JsonObject>) : ServerEvent
    data object PartnerDefaults : ServerEvent

    sealed interface InThread : ServerEvent { val session: String }

    @Serializable
    data class ComposerNotice(override val session: String, val text: String) : InThread

    @Serializable
    data class UserPrompt(override val session: String, val item: String, val text: String,
                          val paths: List<String> = emptyList(),
                          @SerialName("to_worker") val toWorker: Boolean = false) : InThread

    @Serializable
    data class TextDelta(override val session: String, val item: String, val actor: Actor, val text: String) : InThread

    @Serializable
    data class ToolCall(override val session: String, val item: String, val actor: Actor,
                        val name: String, val input: JsonElement) : InThread

    @Serializable
    data class ToolResult(override val session: String, val item: String, val ok: Boolean, val output: String) : InThread

    data class ApprovalRecorded(override val session: String, val receipt: ApprovalReceipt) : InThread

    @Serializable
    data class DelegateProgress(override val session: String, val job: String, val summary: String) : InThread

    @Serializable
    data class DelegateRenamed(override val session: String, val job: String, val change: JobNameChange) : InThread

    @Serializable
    data class PlanChanged(override val session: String, val plan: Plan, val progress: GoalProgress? = null) : InThread

    @Serializable
    data class ApprovalRequested(override val session: String, @SerialName("request_id") val requestId: String,
                                 val summary: String,
                                 @SerialName("shell_directory") val shellDirectory: String? = null,
                                 @SerialName("shell_prefix") val shellPrefix: String? = null) : InThread {
        val isFolderAccess: Boolean get() = requestId.startsWith(FOLDER_ACCESS_REQUEST)
    }

    @Serializable
    data class ApprovalResolved(@SerialName("request_id") val requestId: String) : ServerEvent

    @Serializable data class ReplySuperseded(override val session: String, val items: Set<String>) : InThread
    @Serializable data class TurnActivity(override val session: String, val running: Boolean) : InThread
    @Serializable data class TurnEnded(override val session: String, val actor: Actor) : InThread
    @Serializable data class Paused(override val session: String, val reason: String) : InThread
    @Serializable data class Resumed(override val session: String) : InThread
}

@Serializable
data class ServerUpdateStatus(val active: Boolean, val cancelled: Boolean = false,
                              val error: String? = null, val steps: List<ServerUpdateStep> = emptyList())

@Serializable
data class ServerUpdateStep(val kind: String, val state: String)

data class ServerFrame(val workspace: String?, val event: ServerEvent, val seq: ULong)

internal fun composerPrompt(session: String, text: String, paths: List<String>): ClientMessage =
    if (paths.isEmpty()) ClientMessage.Prompt(session, text)
    else ClientMessage.PromptWithAttachments(session, text, paths)

object Protocol {
    internal val json = Json { ignoreUnknownKeys = true; encodeDefaults = true; explicitNulls = false }

    @Serializable
    private data class Envelope(val seq: ULong, val workspace: String? = null, val event: JsonObject)

    fun encode(message: ClientMessage): String = json.encodeToString(ClientMessage.serializer(), message)

    fun decode(text: String): ServerFrame {
        val envelope = json.decodeFromString<Envelope>(text)
        return ServerFrame(envelope.workspace, decodeEvent(envelope.event), envelope.seq)
    }

    internal fun decodeEvent(event: JsonObject): ServerEvent {
        val type = (event["type"] as? JsonPrimitive)?.takeIf { it.isString }?.content
        return when (type) {
            "welcome" -> json.decodeFromJsonElement<ServerEvent.Welcome>(event)
            "approval_queue" -> json.decodeFromJsonElement<ServerEvent.ApprovalQueue>(event)
            "approval_queue_changed" -> json.decodeFromJsonElement<ServerEvent.ApprovalQueueChanged>(event)
            "approval_decision_result" -> json.decodeFromJsonElement<ServerEvent.ApprovalDecisionResult>(event)
            "workspace_selected" -> json.decodeFromJsonElement<ServerEvent.WorkspaceSelected>(event)
            "workspaces" -> json.decodeFromJsonElement<ServerEvent.Workspaces>(event)
            "chat_activity" -> json.decodeFromJsonElement<ServerEvent.ChatActivity>(event)
            "thread_sync" -> json.decodeFromJsonElement<ServerEvent.ThreadSync>(event)
            "thread_history_start" -> json.decodeFromJsonElement<ServerEvent.ThreadHistoryStart>(event)
            "thread_tail" -> json.decodeFromJsonElement<ServerEvent.ThreadTail>(event)
            "thread_job_details" -> json.decodeFromJsonElement<ServerEvent.ThreadJobDetails>(event)
            "thread_job_activity" -> json.decodeFromJsonElement<ServerEvent.ThreadJobActivity>(event)
            "thread_jobs" -> json.decodeFromJsonElement<ServerEvent.ThreadJobs>(event)
            "thread_job_updated" -> json.decodeFromJsonElement<ServerEvent.ThreadJobUpdated>(event)
            "thread_history_page" -> json.decodeFromJsonElement<ServerEvent.ThreadHistoryPage>(event)
            "thread_history_failed" -> json.decodeFromJsonElement<ServerEvent.ThreadHistoryFailed>(event)
            "thread_history_invalidated" -> json.decodeFromJsonElement<ServerEvent.ThreadHistoryInvalidated>(event)
            "thread_restored" -> json.decodeFromJsonElement<ServerEvent.ThreadRestored>(event)
            "reply_superseded" -> json.decodeFromJsonElement<ServerEvent.ReplySuperseded>(event)
            "threads" -> json.decodeFromJsonElement<ServerEvent.Threads>(event)
            "thread_created" -> json.decodeFromJsonElement<ServerEvent.ThreadCreated>(event)
            "notify" -> json.decodeFromJsonElement<ServerEvent.Notify>(event)
            "worker_approval_policy_changed" -> json.decodeFromJsonElement<ServerEvent.WorkerApprovalPolicyChanged>(event)
            "self_update_status" -> json.decodeFromJsonElement<ServerEvent.UpdateChanged>(event)
            "error", "workspace_error" -> json.decodeFromJsonElement<ServerEvent.Error>(event)
            "chat_waiting" -> json.decodeFromJsonElement<ServerEvent.WaitingChanged>(event)
            "drain_status" -> json.decodeFromJsonElement<ServerEvent.DrainChanged>(event)
            "partner_default_changed" -> ServerEvent.PartnerDefaults
            "local_notice" -> if ((event["item"] as? JsonPrimitive)?.content == COMPOSER_NOTICE_ITEM) {
                json.decodeFromJsonElement<ServerEvent.ComposerNotice>(event)
            } else {
                val localized = event["localized"] as? JsonObject
                val session = (event["session"] as? JsonPrimitive)?.content
                val receipt = ((localized?.get("args") as? JsonObject)?.get("receipt") as? JsonPrimitive)?.content
                    ?.takeIf { (localized?.get("key") as? JsonPrimitive)?.content == "approval-receipt" }
                    ?.let { runCatching { json.decodeFromString<ApprovalReceipt>(it) }.getOrNull() }
                if (session != null && receipt != null) ServerEvent.ApprovalRecorded(session, receipt)
                else ServerEvent.Unknown(type)
            }
            "user_prompt" -> json.decodeFromJsonElement<ServerEvent.UserPrompt>(event)
            "attachment_progress" -> json.decodeFromJsonElement<ServerEvent.AttachmentProgress>(event)
            "attachment_uploaded" -> json.decodeFromJsonElement<ServerEvent.AttachmentUploaded>(event)
            "text_delta" -> json.decodeFromJsonElement<ServerEvent.TextDelta>(event)
            "tool_call" -> json.decodeFromJsonElement<ServerEvent.ToolCall>(event)
            "tool_result" -> json.decodeFromJsonElement<ServerEvent.ToolResult>(event)
            "delegate_progress" -> json.decodeFromJsonElement<ServerEvent.DelegateProgress>(event)
            "delegate_renamed" -> json.decodeFromJsonElement<ServerEvent.DelegateRenamed>(event)
            "plan_changed" -> json.decodeFromJsonElement<ServerEvent.PlanChanged>(event)
            "approval_requested" -> json.decodeFromJsonElement<ServerEvent.ApprovalRequested>(event)
            "approval_resolved" -> json.decodeFromJsonElement<ServerEvent.ApprovalResolved>(event)
            "turn_activity" -> json.decodeFromJsonElement<ServerEvent.TurnActivity>(event)
            "turn_ended" -> json.decodeFromJsonElement<ServerEvent.TurnEnded>(event)
            "paused" -> json.decodeFromJsonElement<ServerEvent.Paused>(event)
            "resumed" -> json.decodeFromJsonElement<ServerEvent.Resumed>(event)
            else -> ServerEvent.Unknown(type)
        }
    }
}
