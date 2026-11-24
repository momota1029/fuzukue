package dev.labgui.remote

import java.io.InputStream
import java.io.OutputStream
import kotlin.math.roundToInt

internal const val MAX_ATTACHMENTS = 8
internal const val MAX_ATTACHMENT_BYTES = 50L * 1024 * 1024
internal const val ATTACHMENT_CHUNK_BYTES = 512 * 1024
internal const val MAX_IMAGE_SOURCE_BYTES = 2 * MAX_ATTACHMENT_BYTES

internal enum class AttachmentProblem(val retryable: Boolean = false) {
    TooLarge, ImageDimensions, InvalidImage, UnsupportedFile, Read(true), Disconnected(true), Upload(true),
}

internal enum class AttachmentSizing { Original, Reencode, Downscale, Reject }

internal fun attachmentSizing(size: Long, image: Boolean, hasLocation: Boolean = false) = when {
    size > MAX_IMAGE_SOURCE_BYTES || (size > MAX_ATTACHMENT_BYTES && !image) -> AttachmentSizing.Reject
    size > MAX_ATTACHMENT_BYTES -> AttachmentSizing.Downscale
    image && hasLocation -> AttachmentSizing.Reencode
    else -> AttachmentSizing.Original
}

internal fun fitAttachmentImage(width: Int, height: Int, maxEdge: Int): Pair<Int, Int> {
    val scale = minOf(1.0, maxEdge.toDouble() / maxOf(width, height))
    return (width * scale).roundToInt().coerceAtLeast(1) to
        (height * scale).roundToInt().coerceAtLeast(1)
}

internal fun validAttachmentName(name: String): Boolean =
    name.isNotEmpty() && !name.startsWith('.') && name.toByteArray(Charsets.UTF_8).size <= 240 &&
        name.none { it in "/\\:" || it.isISOControl() }

internal fun safeAttachmentName(displayName: String?, extension: String? = null): String {
    val cleaned = displayName.orEmpty().map { char ->
        if (char in "/\\:<>|\"?*" || char.isISOControl()) '_' else char
    }.joinToString("").trim { it == '.' || it.isWhitespace() }
    val suffix = extension ?: cleaned.substringAfterLast('.', "").takeIf {
        it.length in 1..12 && it.all { char -> char in 'a'..'z' || char in 'A'..'Z' || char in '0'..'9' }
    }
    val stem = if (suffix == null) cleaned else cleaned.substringBeforeLast('.', cleaned)
    // Reserve space for the server's collision suffix and keep UTF-8 characters whole.
    val bounded = StringBuilder()
    var bytes = 0
    var index = 0
    while (index < stem.length) {
        val codePoint = stem.codePointAt(index)
        val character = String(Character.toChars(codePoint))
        bytes += character.toByteArray(Charsets.UTF_8).size
        if (bytes > 220) break
        bounded.append(character)
        index += Character.charCount(codePoint)
    }
    val base = bounded.toString().trimEnd(' ', '.').ifEmpty { "attachment" }
    return if (suffix == null) base else "$base.$suffix"
}

internal fun validInboxPath(path: String): Boolean {
    val parts = path.replace('\\', '/').split('/')
    return parts.size == 3 && parts[0] == "inbox" && inboxDate.matches(parts[1]) &&
        validAttachmentName(parts[2])
}

private val inboxDate = Regex("[0-9]{4}-[0-9]{2}-[0-9]{2}")

internal enum class AttachmentImage(val extension: String, vararg val mimeTypes: String) {
    Png("png", "image/png"),
    Jpeg("jpg", "image/jpeg", "image/jpg"),
    Gif("gif", "image/gif"),
    Webp("webp", "image/webp"),
    Bmp("bmp", "image/bmp", "image/x-ms-bmp"),
    Heif("heic", "image/heic", "image/heif", "image/heic-sequence", "image/heif-sequence"),
    Avif("avif", "image/avif", "image/avif-sequence");

    fun matchesMime(mime: String?) = mime == null || mime in genericAttachmentMimes ||
        mime == "image/*" || mime in mimeTypes
}

internal val genericAttachmentMimes = setOf("application/octet-stream", "*/*")
internal val documentAttachmentMimes = arrayOf(
    "image/*", "application/pdf", "application/json", "text/*",
)

internal fun isDocumentAttachment(mime: String?): Boolean =
    mime == "application/pdf" || mime == "application/json" || mime?.startsWith("text/") == true

internal fun attachmentImage(header: ByteArray): AttachmentImage? {
    fun ascii(offset: Int, count: Int) =
        if (offset + count <= header.size) String(header, offset, count, Charsets.US_ASCII) else ""
    return when {
        header.size >= 8 && header.take(8) == listOf<Byte>(-119, 80, 78, 71, 13, 10, 26, 10) ->
            AttachmentImage.Png
        header.size >= 3 && header[0].toInt() == -1 && header[1].toInt() == -40 &&
            header[2].toInt() == -1 -> AttachmentImage.Jpeg
        ascii(0, 6) in setOf("GIF87a", "GIF89a") -> AttachmentImage.Gif
        ascii(0, 4) == "RIFF" && ascii(8, 4) == "WEBP" -> AttachmentImage.Webp
        ascii(0, 2) == "BM" -> AttachmentImage.Bmp
        ascii(4, 4) == "ftyp" -> {
            val brands = listOf(ascii(8, 4)) + (16 until header.size - 3 step 4).map { ascii(it, 4) }
            when {
                brands.any { it == "avif" || it == "avis" } -> AttachmentImage.Avif
                brands.any { it in setOf("heic", "heix", "hevc", "hevx", "mif1", "msf1") } -> AttachmentImage.Heif
                else -> null
            }
        }
        else -> null
    }
}

// The extra byte distinguishes an exact limit from a provider that omitted or misstated its size.
internal fun copyAttachment(input: InputStream, output: OutputStream, limit: Long): Boolean {
    val buffer = ByteArray(64 * 1024)
    var remaining = limit
    while (remaining > 0) {
        val count = input.read(buffer, 0, minOf(remaining, buffer.size.toLong()).toInt())
        if (count == -1) return true
        output.write(buffer, 0, count)
        remaining -= count
    }
    return input.read() == -1
}

internal data class UploadCursor(val size: Long, val offset: Long = 0) {
    init {
        require(size in 0..MAX_ATTACHMENT_BYTES && offset in 0..size)
    }

    val chunkSize: Int get() = minOf(size - offset, ATTACHMENT_CHUNK_BYTES.toLong()).toInt()
    val end: Long get() = offset + chunkSize
    val finalChunk: Boolean get() = end == size

    fun acknowledge(next: Long): UploadCursor? =
        if (!finalChunk && next == end) copy(offset = next) else null
}

internal sealed interface AttachmentStatus {
    data object Pending : AttachmentStatus
    data class Uploading(val cursor: UploadCursor) : AttachmentStatus
    data class Done(val path: String) : AttachmentStatus
    data class Failed(val problem: AttachmentProblem) : AttachmentStatus
}

internal sealed interface AttachmentEvent {
    data class Start(val size: Long) : AttachmentEvent
    data class Progress(val offset: Long) : AttachmentEvent
    data class Uploaded(val path: String?) : AttachmentEvent
    data class Fail(val problem: AttachmentProblem) : AttachmentEvent
    data object Retry : AttachmentEvent
    data object Remove : AttachmentEvent
}

internal fun AttachmentStatus.advance(event: AttachmentEvent): AttachmentStatus? = when (event) {
    is AttachmentEvent.Start ->
        if (this == AttachmentStatus.Pending) AttachmentStatus.Uploading(UploadCursor(event.size)) else this
    is AttachmentEvent.Progress -> if (this is AttachmentStatus.Uploading) {
        cursor.acknowledge(event.offset)?.let { AttachmentStatus.Uploading(it) }
            ?: AttachmentStatus.Failed(AttachmentProblem.Upload)
    } else this
    is AttachmentEvent.Uploaded -> if (this is AttachmentStatus.Uploading) {
        val path = event.path
        if (cursor.finalChunk && path != null && validInboxPath(path)) AttachmentStatus.Done(path.replace('\\', '/'))
        else AttachmentStatus.Failed(AttachmentProblem.Upload)
    } else this
    is AttachmentEvent.Fail ->
        if (this is AttachmentStatus.Done) this else AttachmentStatus.Failed(event.problem)
    AttachmentEvent.Retry ->
        if (this is AttachmentStatus.Failed && problem.retryable) AttachmentStatus.Pending else this
    AttachmentEvent.Remove -> null
}

internal data class ComposerActions(val primaryStop: Boolean, val secondaryStop: Boolean, val canSend: Boolean)

internal fun isComposerCommand(text: String): Boolean = text.startsWith('/') && !text.startsWith("//")

internal fun composerActions(
    running: Boolean, connected: Boolean, hasText: Boolean, hasAttachments: Boolean, attachmentsReady: Boolean,
): ComposerActions {
    val hasDraft = hasText || hasAttachments
    return ComposerActions(
        primaryStop = running && !hasDraft,
        secondaryStop = running && hasDraft,
        canSend = connected && hasDraft && attachmentsReady,
    )
}
