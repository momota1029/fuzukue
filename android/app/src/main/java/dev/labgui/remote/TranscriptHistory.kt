package dev.labgui.remote

import kotlinx.serialization.json.JsonObject

internal data class TranscriptUpdate(val seq: ULong, val event: ServerEvent.InThread)

internal val ServerEvent.InThread.isSavedTranscriptEvent: Boolean
    get() = when (this) {
        is ServerEvent.TextDelta, is ServerEvent.UserPrompt, is ServerEvent.ToolCall, is ServerEvent.ToolResult,
        is ServerEvent.DelegateProgress, is ServerEvent.DelegateRenamed, is ServerEvent.ThreadJobActivity,
        is ServerEvent.ReplySuperseded, is ServerEvent.TurnEnded -> true
        else -> false
    }

private fun ServerEvent.InThread.rowKey(): String? = when (this) {
    is ServerEvent.TextDelta -> if (actor.kind == "user") "user/$item" else item
    is ServerEvent.UserPrompt -> "user/$item"
    is ServerEvent.ToolCall -> item
    is ServerEvent.ToolResult -> item
    is ServerEvent.DelegateProgress -> job
    is ServerEvent.DelegateRenamed -> job
    is ServerEvent.ThreadJobActivity -> job
    else -> null
}

internal fun savedTranscriptEvents(session: String, events: List<JsonObject>) =
    events.mapNotNull { Protocol.decodeEvent(it) as? ServerEvent.InThread }
        .filter { it.session == session && it.isSavedTranscriptEvent }

internal class TranscriptHistory {
    var jobsRequested = false
    var generation: ULong? = null
        private set
    private var tailThrough = 0uL
    private val pageThrough = mutableMapOf<String, ULong>()
    var before: ULong? = null
        private set
    private var sparseWorkers = emptyList<String>()

    var tail: Pending? = null
    var page: PendingPage? = null
    var pageInterrupted = false
        private set
    private val pageUpdates = mutableListOf<TranscriptUpdate>()

    class Pending(val request: ULong, val updates: MutableList<TranscriptUpdate> = mutableListOf())
    class PendingPage(val request: ULong, val before: ULong, val generation: ULong?)

    fun startLegacy(event: ServerEvent.ThreadHistoryStart) {
        before = event.before
        sparseWorkers = event.workers
    }

    fun applyTail(
        event: ServerEvent.ThreadTail, timeline: Timeline, pending: Pending, events: List<ServerEvent.InThread>,
    ): Timeline {
        tail = null
        generation = event.generation
        tailThrough = event.through
        before = event.nextBefore
        sparseWorkers = event.workers
        val snapshot = events.fold(Timeline(), Timeline::reduceSaved)
        val loaded = timeline.clearHistory().prependHistory(snapshot, emptyList())
        return applyUpdates(loaded, pending.updates, savedOnly = true)
    }

    fun applyPage(event: ServerEvent.ThreadHistoryPage, timeline: Timeline): Timeline? {
        val requested = page?.takeIf {
            it.request == event.request && it.before == before &&
                it.generation == event.generation && (it.generation != null) == (event.through != null)
        } ?: return null
        before = event.nextBefore?.takeIf { it < requested.before }
        val older = savedTranscriptEvents(event.session, event.events).fold(Timeline(), Timeline::reduceSaved)
        val merged = timeline.prependHistory(older, sparseWorkers)
        sparseWorkers = sparseWorkers.filterNot { it in older.rows }

        event.through?.let { through ->
            for (key in older.rows.keys) {
                if (key !in timeline.rows) pageThrough[key] = through
            }
        }
        // A reply superseded in flight may have been absent when first removed.
        val updated = finishPage(merged)
        return if (updated.rows.keys == timeline.rows.keys) updated
            else updated.copy(olderPages = timeline.olderPages + 1)
    }

    fun interruptPage() {
        if (page != null && generation != null) pageInterrupted = true
        page = null
    }

    fun finishPage(timeline: Timeline, endOfHistory: Boolean = false): Timeline {
        page = null
        pageInterrupted = false
        if (endOfHistory) before = null
        val updated = applyUpdates(timeline, pageUpdates, savedOnly = true)
        pageUpdates.clear()
        return updated
    }

    fun applyUpdate(timeline: Timeline, update: TranscriptUpdate, savedOnly: Boolean = false): Timeline {
        val (seq, event) = update
        val item = event.rowKey()
        if (seq != 0uL && seq <= maxOf(tailThrough, item?.let(pageThrough::get) ?: 0uL)) return timeline
        if (seq != 0uL && pageThrough.isNotEmpty() && pageThrough.values.all { it < seq }) pageThrough.clear()
        if (generation != null && (page != null || pageInterrupted)) {
            val missing = item != null && item !in timeline.rows
            if (missing || event is ServerEvent.ReplySuperseded) pageUpdates.add(update)
            if (missing) return if (savedOnly) timeline else timeline.observeActivity(event)
        }
        return if (savedOnly) timeline.reduceSaved(event) else timeline.reduce(event)
    }

    private fun applyUpdates(
        timeline: Timeline, updates: List<TranscriptUpdate>, savedOnly: Boolean = false,
    ): Timeline = updates.fold(timeline) { current, update -> applyUpdate(current, update, savedOnly) }
}
