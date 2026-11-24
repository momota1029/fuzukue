package dev.labgui.remote

private const val SILENCE_SENTINEL = "<silent/>"

internal fun isSilentAssistantText(text: String): Boolean {
    val trimmed = text.trim()
    return trimmed.isEmpty() || trimmed == SILENCE_SENTINEL
}

internal fun isIncompleteSilenceSentinel(text: String): Boolean {
    val trimmed = text.trim()
    return trimmed.length in 1 until SILENCE_SENTINEL.length && SILENCE_SENTINEL.startsWith(trimmed)
}
