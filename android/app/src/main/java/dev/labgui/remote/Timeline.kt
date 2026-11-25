package dev.labgui.remote

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.decodeFromJsonElement

private const val BLOCKER_ATTENTION_AGE_NANOS = 30 * 60 * 1_000_000_000L

data class PlanAttention(
    private val firstSeen: Map<String, Long> = emptyMap(),
    private val lastUserPrompt: Long? = null,
) {
    internal fun update(plan: Plan, now: Long): PlanAttention {
        if (plan.status == "done") return copy(firstSeen = emptyMap())
        val reasons = plan.next.mapNotNull { it.blocker?.reason?.trim()?.takeIf(String::isNotEmpty) }.toSet()
        return copy(firstSeen = reasons.associateWith { firstSeen[it] ?: now })
    }

    internal fun recordUserPrompt(now: Long) = copy(lastUserPrompt = now)

    // Plan snapshots have no timestamps. Like desktop, deadlines apply only after a later live prompt.
    internal val expiresAt: Long?
        get() {
            val latestBlocker = firstSeen.values.maxOrNull() ?: return null
            val prompt = lastUserPrompt ?: return null
            return (latestBlocker + BLOCKER_ATTENTION_AGE_NANOS).takeIf { prompt > latestBlocker }
        }

    internal fun isVisible(now: Long): Boolean = firstSeen.isNotEmpty() && (expiresAt?.let { now < it } ?: true)
}

sealed interface TimelineRow {
    data class Message(
        val text: String, val user: Boolean, val userItem: String? = null, val paths: List<String> = emptyList(),
    ) : TimelineRow
    data class Note(val text: String) : TimelineRow
    data class ApprovalOutcome(val approval: QueuedApproval, val decidedHere: Boolean) : TimelineRow
    data class Tool(val label: String, val ok: Boolean? = null) : TimelineRow
    data class Job(
        val info: JobInfo,
        val summary: String? = null,
        val ok: Boolean? = null,
        val report: JobReport? = null,
        val detailRequest: ULong? = null,
        val details: List<String>? = null,
        val nextDetail: ULong? = null,
        val loadingDetails: Boolean = false,
    ) : TimelineRow
}

data class Timeline(
    val rows: Map<String, TimelineRow> = emptyMap(),
    val running: Boolean = false,
    val plan: Plan? = null,
    val goalProgress: GoalProgress? = null,
    val pauseReason: String? = null,
    val runningSince: Long? = null,
    val planAttention: PlanAttention = PlanAttention(),
    private val streamingSilencePrefixes: Set<String> = emptySet(),
    val composerNotice: String? = null,
    internal val olderPages: Int = 0,
) {
    internal val visibleRows: Map<String, TimelineRow>
        get() = rows.filterNot { (id, row) ->
            row is TimelineRow.Message && !row.user &&
                (id in streamingSilencePrefixes || isSilentAssistantText(row.text))
        }

    fun reduce(
        event: ServerEvent.InThread,
        now: Long = System.currentTimeMillis(),
        replaying: Boolean = false,
        attentionNow: Long = System.nanoTime(),
    ): Timeline = when (event) {
        is ServerEvent.ComposerNotice -> if (replaying || composerNotice == event.text) this
            else copy(composerNotice = event.text)
        is ServerEvent.UserPrompt -> {
            val updated = appendUser(event.item, event.text, event.paths)
            if (replaying) updated else updated.copy(planAttention = planAttention.recordUserPrompt(attentionNow))
        }
        is ServerEvent.TextDelta -> when {
            event.actor.kind == "user" -> appendUser(event.item, event.text)
            event.actor.kind == "system" && event.item.startsWith("thread-plan:") ->
                withRow(event.item, TimelineRow.Note(event.text))
            childJob(event.item, event.actor) != null -> appendJobDetail(childJob(event.item, event.actor), event.text)
            else -> {
                val previous = rows[event.item] as? TimelineRow.Message
                val text = previous?.text.orEmpty() + event.text
                val updated = withRow(event.item, TimelineRow.Message(text, user = false))
                val prefixes = when {
                    event.actor.isPartner && isIncompleteSilenceSentinel(text) -> streamingSilencePrefixes + event.item
                    event.item in streamingSilencePrefixes -> streamingSilencePrefixes - event.item
                    else -> streamingSilencePrefixes
                }
                val streamed = updated.copy(streamingSilencePrefixes = prefixes)
                if (event.actor.isPartner) streamed.withRunning(true, now) else streamed
            }
        }
        is ServerEvent.ToolCall -> when {
            event.item.startsWith("delegate:") && event.name == "delegate" -> {
                val info = runCatching { Protocol.json.decodeFromJsonElement<JobInfo>(event.input) }.getOrNull()
                if (info == null) this
                else {
                    val previous = rows[event.item] as? TimelineRow.Job
                    withRow(event.item, previous?.copy(info = info) ?: TimelineRow.Job(info))
                }
            }
            childJob(event.item, event.actor) != null -> appendJobDetail(
                childJob(event.item, event.actor), "${event.name}\n${event.input}",
            )
            else -> {
                val input = event.input as? JsonObject
                val detail = listOf("command", "file_path", "path", "file")
                    .firstNotNullOfOrNull { (input?.get(it) as? JsonPrimitive)?.content }
                val label = if (detail == null) event.name else "${event.name} ${detail.lineSequence().first().take(120)}"
                withRow(event.item, TimelineRow.Tool(label))
            }
        }
        is ServerEvent.ToolResult -> when (val previous = rows[event.item]) {
            is TimelineRow.Job -> {
                val report = runCatching { Protocol.json.decodeFromString<JobReport>(event.output) }.getOrNull()
                withRow(event.item, previous.copy(ok = event.ok, report = report))
            }
            is TimelineRow.Tool -> withRow(event.item, previous.copy(ok = event.ok))
            else -> appendJobDetail(childJob(event.item), event.output)
        }
        is ServerEvent.ReplySuperseded -> copy(
            rows = rows - event.items,
            streamingSilencePrefixes = streamingSilencePrefixes - event.items,
        )
        is ServerEvent.ThreadJobActivity -> {
            val previous = rows[event.job] as? TimelineRow.Job
            if (previous == null) this else withRow(event.job, previous.copy(info = previous.info.copy(toolCount = event.tools)))
        }
        is ServerEvent.DelegateRenamed -> {
            val previous = rows[event.job] as? TimelineRow.Job
            if (previous == null) this else withRow(event.job, previous.copy(info = previous.info.copy(label = event.change.label)))
        }
        is ServerEvent.DelegateProgress -> {
            val previous = rows[event.job] as? TimelineRow.Job
            if (previous == null) this else withRow(event.job, previous.copy(summary = event.summary))
        }
        is ServerEvent.PlanChanged -> copy(
            plan = event.plan,
            goalProgress = event.progress,
            planAttention = planAttention.update(event.plan, attentionNow),
        )
        is ServerEvent.TurnActivity -> withRunning(event.running, now).let {
            if (event.running) it.copy(composerNotice = null) else it
        }
        is ServerEvent.TurnEnded -> if (event.actor.isPartner) withRunning(false, now).copy(composerNotice = null)
            else this
        is ServerEvent.Paused -> copy(pauseReason = event.reason)
        is ServerEvent.Resumed -> copy(pauseReason = null)
        is ServerEvent.ApprovalRequested -> this
    }

    internal fun updateJob(job: String, update: (TimelineRow.Job) -> TimelineRow.Job): Timeline {
        val row = rows[job] as? TimelineRow.Job ?: return this
        return withRow(job, update(row))
    }

    private fun appendJobDetail(job: String?, text: String): Timeline {
        if (job == null || text.isBlank()) return this
        return updateJob(job) { row ->
            val details = row.details ?: return@updateJob row
            row.copy(details = details.takeLast(127) + text.take(32 * 1024))
        }
    }

    internal fun withActivity(running: Boolean): Timeline = withRunning(running, System.currentTimeMillis())

    internal fun observeActivity(event: ServerEvent.InThread): Timeline = when {
        event is ServerEvent.TextDelta && event.actor.isPartner -> withActivity(true)
        event is ServerEvent.TurnEnded && event.actor.isPartner -> withActivity(false).copy(composerNotice = null)
        else -> this
    }

    internal fun reduceSaved(event: ServerEvent.InThread): Timeline {
        val updated = reduce(event, replaying = true)
        return copy(rows = updated.rows, streamingSilencePrefixes = updated.streamingSilencePrefixes)
    }

    internal fun recordApproval(approval: QueuedApproval, decidedHere: Boolean): Timeline {
        val id = "async-approval/${approval.id}/${approval.state}"
        val latest = rows.values.lastOrNull { it is TimelineRow.ApprovalOutcome && it.approval.id == approval.id }
            as? TimelineRow.ApprovalOutcome
        if (latest != null && latest.approval.updatedAt > approval.updatedAt) return this
        val previous = rows[id] as? TimelineRow.ApprovalOutcome
        if (previous != null && (!decidedHere || previous.decidedHere)) return this
        return withRow(id, TimelineRow.ApprovalOutcome(approval, decidedHere))
    }

    // Queue outcomes arrive on the live subscription, outside paged transcript history.
    internal fun clearHistory() = copy(
        rows = rows.filterValues { it is TimelineRow.ApprovalOutcome },
        streamingSilencePrefixes = emptySet(),
    )

    internal fun prependHistory(page: Timeline, sparseWorkers: List<String>): Timeline {
        // A sparse worker can sit between rows from the same page. Its live
        // progress and result remain authoritative when the page crosses it.
        val sparse = sparseWorkers.toSet()
        val batches = mutableMapOf<String, Map<String, TimelineRow>>()
        var batch = linkedMapOf<String, TimelineRow>()
        for ((key, row) in page.rows) {
            if (key in sparse && key in rows) {
                batches[key] = batch
                batch = linkedMapOf()
            } else if (key !in rows) {
                batch[key] = row
            }
        }
        val firstContiguous = rows.keys.firstOrNull { it !in sparse }
        val merged = buildMap {
            for ((key, row) in rows) {
                batches[key]?.let { putAll(it) }
                if (key == firstContiguous) putAll(batch)
                put(key, row)
            }
            if (firstContiguous == null) putAll(batch)
        }
        return copy(
            rows = merged,
            streamingSilencePrefixes = streamingSilencePrefixes +
                page.streamingSilencePrefixes.filter { it !in rows },
        )
    }

    private fun withRow(id: String, row: TimelineRow) = copy(rows = rows + (id to row))

    private fun withRunning(value: Boolean, now: Long) =
        copy(
            running = value,
            runningSince = if (value) runningSince ?: now else null,
            streamingSilencePrefixes = if (value) streamingSilencePrefixes else emptySet(),
        )

    private fun appendUser(id: String, text: String, paths: List<String> = emptyList()): Timeline {
        // Some older transcripts reuse user IDs after a server restart.
        var key = "user/$id"
        while (key in rows) key += "/"
        return withRow(key, TimelineRow.Message(text, user = true, userItem = id, paths = paths))
    }
}

private fun childJob(item: String, actor: Actor? = null): String? =
    actor?.parentToolUse?.takeIf { it.startsWith("delegate:") }
        ?: item.removePrefix("delegate:").takeIf { item.startsWith("delegate:") && ':' in it }
            ?.substringBefore(':')?.let { "delegate:$it" }
