package dev.labgui.remote

import kotlinx.serialization.json.JsonElement
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
    data class ApprovalDenial(val tool: String) : TimelineRow
    data class Tool(
        val label: String, val ok: Boolean? = null, val name: String = label, val actor: Actor? = null,
        val item: String = "", val input: JsonElement? = null, val output: String? = null,
    ) : TimelineRow
    data class ToolRun(
        val tools: List<Pair<Tool, Int>>,
        val unlinkedApprovals: Int,
    ) : TimelineRow {
        val approvalCount: Int get() = unlinkedApprovals + tools.sumOf { it.second }
    }
    data class Job(
        val info: JobInfo,
        val summary: String? = null,
        val ok: Boolean? = null,
        val report: JobReport? = null,
        val detailRequest: ULong? = null,
        val details: List<JobDetail>? = null,
        val nextDetail: ULong? = null,
        val loadingDetails: Boolean = false,
        val wireSummary: JobSummary? = null,
    ) : TimelineRow
}

sealed interface JobDetail {
    data class Text(val text: String) : JobDetail
    data class Tool(val row: TimelineRow.Tool) : JobDetail
}

internal fun ServerEvent.ToolCall.toolRow(): TimelineRow.Tool {
    val fields = input as? JsonObject
    val detail = listOf("command", "file_path", "path", "file")
        .firstNotNullOfOrNull { (fields?.get(it) as? JsonPrimitive)?.content }
    val label = if (detail == null) name else name + " " + detail.lineSequence().first().take(120)
    return TimelineRow.Tool(label, name = name, actor = actor, item = item, input = input)
}

internal fun jobDetailPage(events: List<ServerEvent>, job: String): List<JobDetail> {
    val details = mutableListOf<JobDetail>()
    val tools = mutableMapOf<String, Int>()
    fun text(value: String?) {
        if (!value.isNullOrBlank()) details += JobDetail.Text(value)
    }
    for (event in events) {
        when (event) {
            is ServerEvent.TextDelta -> text(event.text)
            is ServerEvent.UserPrompt -> text(event.text)
            is ServerEvent.DelegateProgress -> text(event.summary)
            is ServerEvent.ToolCall -> if (event.item != job) {
                tools[event.item] = details.size
                details += JobDetail.Tool(event.toolRow())
            }
            is ServerEvent.ToolResult -> if (event.item == job) {
                text(runCatching { Protocol.json.decodeFromString<JobReport>(event.output).text }.getOrNull())
            } else {
                val index = tools[event.item]
                if (index == null) text(event.output)
                else {
                    val row = (details[index] as JobDetail.Tool).row
                    details[index] = JobDetail.Tool(row.copy(ok = event.ok, output = event.output))
                }
            }
            else -> Unit
        }
    }
    return details
}

enum class HistoryEdge { Unknown, More, Loading, Start }

data class Timeline(
    val rows: Map<String, TimelineRow> = emptyMap(),
    val running: Boolean = false,
    val plan: Plan? = null,
    val goalProgress: GoalProgress? = null,
    val pauseReason: String? = null,
    val runningSince: Long? = null,
    val planAttention: PlanAttention = PlanAttention(),
    internal val streamingSilencePrefixes: Set<String> = emptySet(),
    val composerNotice: String? = null,
    internal val olderPages: Int = 0,
    val jobSummaries: JobSummaries = JobSummaries(),
    val historyEdge: HistoryEdge = HistoryEdge.Unknown,
    val approvalReceipts: Map<String, ApprovalReceipt> = emptyMap(),
) {
    internal val visibleRows: Map<String, TimelineRow>
        get() = rows.filterNot { (id, row) ->
            row is TimelineRow.Message && !row.user &&
                (id in streamingSilencePrefixes || isSilentAssistantText(row.text))
        }

    internal val displayRows: Map<String, TimelineRow>
        get() = groupToolRows(visibleRows, approvalReceipts.values)

    fun reduce(
        event: ServerEvent.InThread,
        now: Long = System.currentTimeMillis(),
        replaying: Boolean = false,
        attentionNow: Long = System.nanoTime(),
    ): Timeline = when (event) {
        is ServerEvent.ApprovalRecorded -> recordApproval(event.receipt)
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
            childJob(event.item, event.actor) != null -> appendJobDetail(childJob(event.item, event.actor), JobDetail.Text(event.text.take(32 * 1024)))
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
                    val summary = (event.input as? JsonObject)?.get("job_summary")?.let {
                        runCatching { Protocol.json.decodeFromJsonElement<JobSummary>(it) }.getOrNull()
                    }
                    withRow(
                        event.item,
                        previous?.copy(info = info, wireSummary = summary ?: previous.wireSummary)
                            ?: TimelineRow.Job(info, wireSummary = summary),
                    )
                }
            }
            childJob(event.item, event.actor) != null -> appendJobDetail(
                childJob(event.item, event.actor), JobDetail.Tool(event.toolRow()),
            )
            else -> withRow(event.item, event.toolRow())
        }
        is ServerEvent.ToolResult -> when (val previous = rows[event.item]) {
            is TimelineRow.Job -> {
                val report = runCatching { Protocol.json.decodeFromString<JobReport>(event.output) }.getOrNull()
                withRow(event.item, previous.copy(ok = event.ok, report = report))
            }
            is TimelineRow.Tool -> withRow(event.item, previous.copy(ok = event.ok, output = event.output))
            else -> {
                val job = childJob(event.item)
                if (job == null) this else updateJob(job) { row ->
                    val details = row.details ?: return@updateJob row
                    val index = details.indexOfLast { it is JobDetail.Tool && it.row.item == event.item }
                    if (index < 0) row.copy(details = details.takeLast(127) + JobDetail.Text(event.output.take(32 * 1024)))
                    else row.copy(details = details.toMutableList().apply {
                        val tool = (this[index] as JobDetail.Tool).row
                        this[index] = JobDetail.Tool(tool.copy(ok = event.ok, output = event.output))
                    })
                }
            }
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

    private fun appendJobDetail(job: String?, detail: JobDetail): Timeline {
        if (job == null || detail is JobDetail.Text && detail.text.isBlank()) return this
        return updateJob(job) { row ->
            val details = row.details ?: return@updateJob row
            row.copy(details = details.takeLast(127) + detail)
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
        return copy(
            rows = updated.rows, streamingSilencePrefixes = updated.streamingSilencePrefixes,
            approvalReceipts = updated.approvalReceipts,
        )
    }

    internal fun recordApproval(receipt: ApprovalReceipt): Timeline {
        val previous = approvalReceipts[receipt.id]
        if (previous == receipt || previous != null && (previous.updatedAt > receipt.updatedAt ||
                previous.updatedAt == receipt.updatedAt && previous.toolItem != null && receipt.toolItem == null)
        ) return this
        return copy(approvalReceipts = approvalReceipts + (receipt.id to receipt))
    }

    internal fun clearHistory() = copy(
        rows = emptyMap(),
        streamingSilencePrefixes = emptySet(),
        historyEdge = HistoryEdge.Unknown,
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
        val timeline = copy(
            rows = merged,
            streamingSilencePrefixes = streamingSilencePrefixes +
                page.streamingSilencePrefixes.filter { it !in rows },
        )
        return page.approvalReceipts.values.fold(timeline, Timeline::recordApproval)
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

data class JobSummaries(
    val received: Boolean = false,
    private val through: ULong = 0uL,
    private val jobs: Map<String, Pair<ULong, JobSummary>> = emptyMap(),
) {
    operator fun get(id: String): JobSummary? = jobs[id]?.second

    val active: List<JobSummary> get() = jobs.values.map { it.second }
        .filter { it.status.active }.sortedBy { it.position }

    internal fun update(revision: ULong, summary: JobSummary): JobSummaries =
        if (revision < through || (jobs[summary.id]?.first ?: 0uL) > revision) this
        else copy(jobs = jobs + (summary.id to (revision to summary)))

    internal fun snapshot(revision: ULong, first: Boolean, summaries: List<JobSummary>): JobSummaries {
        if (revision < through) return this
        val base = if (first) copy(received = true, through = revision, jobs = jobs.filterValues { it.first > revision }) else this
        val latest = summaries.filter { (base.jobs[it.id]?.first ?: 0uL) <= revision }
        return base.copy(jobs = base.jobs + latest.associate { it.id to (revision to it) })
    }
}

private fun childJob(item: String, actor: Actor? = null): String? =
    actor?.parentToolUse?.takeIf { it.startsWith("delegate:") }
        ?: item.removePrefix("delegate:").takeIf { item.startsWith("delegate:") && ':' in it }
            ?.substringBefore(':')?.let { "delegate:$it" }
