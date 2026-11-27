package dev.labgui.remote

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive

internal data class ApprovalDockState(
    val pending: Map<String, QueuedApproval> = emptyMap(),
    val decisions: Map<String, ApprovalDecisionProgress> = emptyMap(),
    val loadError: String? = null,
    val isLoading: Boolean = false,
) {
    fun replace(workspace: String, approvals: List<QueuedApproval>) = copy(
        pending = approvals.filter { it.workspace == workspace && it.state == ApprovalQueueState.Pending }
            .associateBy { it.id },
        loadError = null,
        isLoading = false,
    )

    fun update(approval: QueuedApproval): ApprovalDockState {
        val previous = pending[approval.id]
        if (previous != null && previous.updatedAt > approval.updatedAt) return this
        return copy(
            pending = if (approval.state == ApprovalQueueState.Pending) pending + (approval.id to approval)
                else pending - approval.id,
        )
    }

    // R-59 validates deadlines and broadcasts Expired. The device clock need only render the timestamp.
    fun forThread(session: String): List<QueuedApproval> = pending.values.filter { it.thread == session }

    fun countsByThread(): Map<String, Int> = pending.values.groupingBy { it.thread }.eachCount()
}

internal data class ApprovalDecisionProgress(
    val requests: Map<ULong, QueuedApproval>,
    val succeeded: Int = 0,
    val failures: List<ApprovalDecisionFailure> = emptyList(),
) {
    val isSending: Boolean get() = requests.isNotEmpty()

    fun complete(request: ULong, error: String?): ApprovalDecisionProgress {
        val approval = requests[request] ?: return this
        return copy(
            requests = requests - request,
            succeeded = succeeded + if (error == null) 1 else 0,
            failures = if (error == null) failures else failures + ApprovalDecisionFailure(approval, error),
        )
    }

    fun interrupt(message: String) = copy(
        requests = emptyMap(),
        failures = failures + requests.values.map { ApprovalDecisionFailure(it, message) },
    )
}

internal data class ApprovalDecisionFailure(val approval: QueuedApproval, val message: String)

internal fun QueuedApproval.canBatchWith(other: QueuedApproval): Boolean =
    risk != ApprovalRisk.High && other.risk != ApprovalRisk.High &&
        unattended != "never-unattended" && other.unattended != "never-unattended" &&
        workspace == other.workspace && thread == other.thread &&
        action.tool == other.action.tool && action.scope != JsonNull && action.scope == other.action.scope

internal fun QueuedApprovalAction.targetLabel(): String? {
    val locations = listOf(scope as? JsonObject, args as? JsonObject)
    return listOf("target", "targets", "paths", "path", "file_path", "destination", "cwd", "url", "repo", "command", "directory", "name")
        .firstNotNullOfOrNull { key ->
            locations.firstNotNullOfOrNull { it?.get(key)?.takeUnless { value -> value == JsonNull }?.displayText() }
        }
}

internal fun JsonElement.displayText(): String =
    if (this is JsonPrimitive && isString) content else toString()

/** The readable parts of a deferred patch integration, shown in place of its raw arguments. */
internal data class IntegrateCard(
    val workspace: String,
    val files: List<String>,
    val flagged: List<String>,
    val findings: Int,
    val patch: String,
    val head: String,
) {
    /** Flagged lines beyond the ones the server lists. */
    val unlisted: Int get() = (findings - flagged.size).coerceAtLeast(0)
}

internal fun QueuedApprovalAction.integrateCard(): IntegrateCard? {
    if (tool != "git_integrate_patch") return null
    val values = args as? JsonObject ?: return null
    fun text(value: JsonElement?) = (value as? JsonPrimitive)?.takeIf { it.isString }?.content
    fun strings(key: String) = (values[key] as? JsonArray)?.mapNotNull(::text)
    return IntegrateCard(
        workspace = text((scope as? JsonObject)?.get("workspace")).orEmpty(),
        files = strings("paths") ?: return null,
        flagged = strings("locations").orEmpty(),
        findings = (values["privacy_findings"] as? JsonPrimitive)?.content?.toIntOrNull() ?: 0,
        patch = text(values["patch_sha256"]).orEmpty().take(12),
        head = text(values["head"]).orEmpty().take(12),
    )
}
