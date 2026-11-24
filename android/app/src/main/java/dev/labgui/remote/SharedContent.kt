package dev.labgui.remote

internal const val SHARE_SEND = "android.intent.action.SEND"
internal const val SHARE_SEND_MULTIPLE = "android.intent.action.SEND_MULTIPLE"

internal data class SharedContent(val text: String? = null, val images: List<String> = emptyList()) {
    fun merge(next: SharedContent) = SharedContent(
        text = listOfNotNull(text, next.text).joinToString("\n").ifBlank { null },
        images = (images + next.images).distinct().take(MAX_ATTACHMENTS + 1),
    )
}

internal fun parseSharedContent(
    action: String?, mime: String?, text: CharSequence?, streams: Sequence<String>, clipUris: Sequence<String>,
): SharedContent? {
    if (action != SHARE_SEND && action != SHARE_SEND_MULTIPLE) return null
    val image = mime?.startsWith("image/") == true
    if (!image && mime != "text/plain") return null
    val message = text?.toString()?.takeIf(String::isNotBlank)
    val images = if (image) (streams + clipUris)
        .filter { it.startsWith("content://") }.distinct().take(MAX_ATTACHMENTS + 1).toList()
        else emptyList()
    return SharedContent(message, images).takeIf { message != null || images.isNotEmpty() }
}
