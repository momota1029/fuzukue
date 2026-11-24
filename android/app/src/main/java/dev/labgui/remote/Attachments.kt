package dev.labgui.remote

import android.content.Context
import android.net.Uri
import android.os.Bundle
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.io.RandomAccessFile
import java.util.Base64

internal data class DraftAttachment(
    val id: Long,
    val source: AttachmentSource,
    val status: AttachmentStatus = AttachmentStatus.Pending,
    val prepared: PreparedAttachment? = null,
) {
    val canRetry get() = (status as? AttachmentStatus.Failed)?.problem?.retryable == true &&
        (prepared != null || source.capture == null)
}

internal data class AttachmentDraft(
    val items: List<DraftAttachment> = emptyList(),
    val notice: Int? = null,
    val sharedText: SharedDraftText? = null,
)

internal data class SharedDraftText(val revision: Long, val text: String)

internal data class AttachmentLink(val target: ConversationTarget, val path: String)

internal data class AttachmentView(
    val drafts: Map<ConversationTarget, AttachmentDraft> = emptyMap(),
    val sent: Map<AttachmentLink, PreparedAttachment> = emptyMap(),
    val share: SharedContent? = null,
)

// State and the socket callbacks run on the main looper. File and bitmap work runs off it.
internal class Attachments(
    context: Context,
    private val canUpload: (ConversationTarget) -> Boolean,
    private val awaitUpload: suspend (ConversationTarget) -> Boolean,
    private val send: (ClientMessage) -> Boolean,
) {
    private val files = AttachmentFiles(context)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val preparation = Mutex()
    private val transfers = Mutex()
    private val mutableView = MutableStateFlow(AttachmentView())
    val view = mutableView.asStateFlow()
    private var endpoint: String? = null
    private var epoch = 0L
    private var nextDraft = 1L
    private var nextUpload = 1L
    private var nextShare = 1L
    private var waiting: UploadWait? = null

    private data class UploadWait(
        val upload: Long,
        val draft: Long,
        val reply: CompletableDeferred<AttachmentEvent> = CompletableDeferred(),
    )

    fun useServer(value: String) {
        if (endpoint == value) return
        endpoint = value
        connectionChanged()
        val current = view.value
        current.drafts.values.flatMap { it.items }.forEach(::discardDraftFile)
        mutableView.value = AttachmentView(share = current.share)
    }

    fun connectionChanged() {
        epoch++
        waiting?.reply?.complete(AttachmentEvent.Fail(AttachmentProblem.Disconnected))
    }

    fun offerShare(share: SharedContent) {
        val current = view.value
        mutableView.value = current.copy(share = current.share?.merge(share) ?: share)
    }

    fun dismissShare() {
        mutableView.value = view.value.copy(share = null)
    }

    fun acceptShare(target: ConversationTarget) {
        val share = view.value.share ?: return
        dismissShare()
        share.text?.let { text ->
            editDraft(target) {
                val combined = listOfNotNull(it.sharedText?.text, text).joinToString("\n")
                it.copy(sharedText = SharedDraftText(nextShare++, combined))
            }
        }
        addUris(target, share.images.map(Uri::parse), imageOnly = true)
    }

    fun takeSharedText(target: ConversationTarget): String? {
        val text = view.value.drafts[target]?.sharedText ?: return null
        editDraft(target) { it.copy(sharedText = null) }
        return text.text
    }

    fun addUris(target: ConversationTarget, uris: List<Uri>, imageOnly: Boolean, imeGrant: Bundle? = null) {
        val sources = uris.distinct().filter(files::accepts).map { AttachmentSource(it, imageOnly, imeGrant = imeGrant) }
        if (sources.isNotEmpty()) add(target, sources)
        else if (uris.isNotEmpty()) notice(target, R.string.attachment_read_error)
    }

    fun addCapture(target: ConversationTarget, source: AttachmentSource) = add(target, listOf(source))

    fun notice(target: ConversationTarget, label: Int) {
        editDraft(target) { it.copy(notice = label) }
    }

    fun remove(target: ConversationTarget, id: Long) {
        val item = item(target, id) ?: return
        change(target, id, AttachmentEvent.Remove)
        waiting?.takeIf { it.draft == id }?.reply?.complete(AttachmentEvent.Fail(AttachmentProblem.Upload))
        discardDraftFile(item)
    }

    fun retry(target: ConversationTarget, id: Long) {
        if (item(target, id)?.canRetry != true) return
        change(target, id, AttachmentEvent.Retry)
        runAttachment(target, id)
    }

    fun readyPaths(target: ConversationTarget): List<String>? {
        val items = view.value.drafts[target]?.items.orEmpty()
        val paths = items.mapNotNull { (it.status as? AttachmentStatus.Done)?.path }
        if (paths.size != items.size) return null
        // The server inlines small PNGs and caps their combined size at 16 MiB.
        val inlineLimit = 4L * 1024 * 1024
        val inlineBytes = items.mapNotNull { it.prepared }.filter {
            it.name.endsWith(".png", ignoreCase = true) && it.size <= inlineLimit
        }.sumOf { it.size }
        if (inlineBytes > 4 * inlineLimit) {
            notice(target, R.string.attachment_png_budget)
            return null
        }
        return paths
    }

    fun markSent(target: ConversationTarget, paths: List<String>) {
        if (paths.isEmpty()) return
        val current = view.value
        val draft = current.drafts[target] ?: return
        val sent = draft.items.mapNotNull { item ->
            val path = (item.status as? AttachmentStatus.Done)?.path ?: return@mapNotNull null
            item.prepared?.let { AttachmentLink(target, path) to it }
        }
        // Only small in-memory previews survive upload. Bound the number kept for the timeline.
        mutableView.value = current.copy(sent = (current.sent.entries.map { it.toPair() } + sent).takeLast(96).toMap())
        editDraft(target) { it.copy(items = emptyList(), notice = null) }
    }

    fun progress(id: Long, offset: Long) {
        waiting?.takeIf { it.upload == id }?.reply?.complete(AttachmentEvent.Progress(offset))
    }

    fun uploaded(id: Long, path: String?) {
        waiting?.takeIf { it.upload == id }?.reply?.complete(AttachmentEvent.Uploaded(path))
    }

    private fun add(target: ConversationTarget, sources: List<AttachmentSource>) {
        val current = view.value.drafts[target] ?: AttachmentDraft()
        val room = MAX_ATTACHMENTS - current.items.size
        val added = sources.take(room).map { DraftAttachment(nextDraft++, it) }
        sources.drop(room).forEach { it.capture?.delete() }
        editDraft(target) {
            it.copy(items = it.items + added, notice = R.string.attachment_count_limit.takeIf { sources.size > room })
        }
        added.forEach { runAttachment(target, it.id) }
    }

    private fun runAttachment(target: ConversationTarget, id: Long) {
        scope.launch {
            preparation.withLock {
                val initial = item(target, id) ?: return@withLock
                val staged = initial.prepared?.file
                if (staged != null && withContext(Dispatchers.IO) { staged.isFile }) return@withLock
                if (item(target, id) == null) return@withLock
                if (staged != null) editItem(target, id) { it.copy(prepared = null) }
                try {
                    val prepared = withContext(Dispatchers.IO) { files.prepare(initial.source) }
                    if (item(target, id) == null) discardFile(prepared)
                    else editItem(target, id) { it.copy(prepared = prepared, source = it.source.copy(imeGrant = null)) }
                } catch (error: AttachmentFileException) {
                    change(target, id, AttachmentEvent.Fail(error.problem))
                } catch (_: Exception) {
                    change(target, id, AttachmentEvent.Fail(AttachmentProblem.Read))
                }
            }
            transfers.withLock { upload(target, id) }
        }
    }

    private suspend fun upload(target: ConversationTarget, draftId: Long) {
        val initial = item(target, draftId) ?: return
        val prepared = initial.prepared ?: return
        if (initial.status != AttachmentStatus.Pending) return
        if (!canUpload(target) && !awaitUpload(target)) {
            change(target, draftId, AttachmentEvent.Fail(AttachmentProblem.Disconnected))
            return
        }
        if (item(target, draftId) == null) return
        val uploadId = nextUpload++
        val startedOn = epoch
        var sentChunk = false
        change(target, draftId, AttachmentEvent.Start(prepared.size))
        try {
            while (true) {
                val state = item(target, draftId)?.status as? AttachmentStatus.Uploading ?: break
                val cursor = state.cursor
                val data = withContext(Dispatchers.IO) {
                    val file = prepared.file ?: throw AttachmentFileException(AttachmentProblem.Read)
                    val bytes = ByteArray(cursor.chunkSize)
                    RandomAccessFile(file, "r").use {
                        it.seek(cursor.offset)
                        it.readFully(bytes)
                    }
                    Base64.getEncoder().encodeToString(bytes)
                }
                if (item(target, draftId) == null) break
                val waiter = UploadWait(uploadId, draftId)
                waiting = waiter
                val message = ClientMessage.UploadAttachment(uploadId, prepared.name, cursor.size, cursor.offset, data, target.session)
                val event = if (epoch == startedOn && canUpload(target) && send(message)) {
                    sentChunk = true
                    withTimeoutOrNull(60_000) { waiter.reply.await() } ?: AttachmentEvent.Fail(AttachmentProblem.Upload)
                } else AttachmentEvent.Fail(AttachmentProblem.Disconnected)
                waiting = null
                change(target, draftId, event)
            }
        } catch (_: Exception) {
            change(target, draftId, AttachmentEvent.Fail(AttachmentProblem.Read))
        } finally {
            waiting = null
            if (item(target, draftId)?.status is AttachmentStatus.Done) {
                editItem(target, draftId) { it.copy(prepared = prepared.copy(file = null)) }
                discardFile(prepared)
            } else if (sentChunk && epoch == startedOn && canUpload(target)) {
                send(ClientMessage.CancelAttachment(uploadId))
            }
        }
    }

    private fun discardFile(prepared: PreparedAttachment?) {
        val file = prepared?.file ?: return
        scope.launch(Dispatchers.IO) { file.delete() }
    }

    private fun discardDraftFile(item: DraftAttachment) {
        val file = item.prepared?.file ?: item.source.capture ?: return
        scope.launch(Dispatchers.IO) { file.delete() }
    }

    private fun item(target: ConversationTarget, id: Long): DraftAttachment? =
        view.value.drafts[target]?.items?.firstOrNull { it.id == id }

    private fun change(target: ConversationTarget, id: Long, event: AttachmentEvent) {
        editItem(target, id) { item -> item.status.advance(event)?.let { item.copy(status = it) } }
    }

    private fun editItem(target: ConversationTarget, id: Long, edit: (DraftAttachment) -> DraftAttachment?) {
        editDraft(target) { it.copy(items = it.items.mapNotNull { item -> if (item.id == id) edit(item) else item }) }
    }

    private fun editDraft(target: ConversationTarget, edit: (AttachmentDraft) -> AttachmentDraft) {
        val current = view.value
        val updated = edit(current.drafts[target] ?: AttachmentDraft())
        mutableView.value = current.copy(
            drafts = if (updated == AttachmentDraft()) current.drafts - target else current.drafts + (target to updated),
        )
    }
}
