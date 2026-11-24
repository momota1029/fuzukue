package dev.labgui.remote

import android.content.Intent
import android.net.Uri
import androidx.core.content.IntentCompat

internal fun readSharedContent(intent: Intent): SharedContent? {
    val action = intent.action
    if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return null
    val clip = intent.clipData
    val items = (0 until (clip?.itemCount ?: 0)).asSequence().mapNotNull { clip?.getItemAt(it) }
    val streams = if (action == Intent.ACTION_SEND_MULTIPLE) {
        runCatching { IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java) }
            .getOrNull().orEmpty().filterIsInstance<Uri>()
    } else {
        listOfNotNull(runCatching { IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java) }.getOrNull())
    }
    val text = runCatching { intent.getCharSequenceExtra(Intent.EXTRA_TEXT) }.getOrNull()
        ?: items.firstNotNullOfOrNull { it.text }
    return parseSharedContent(action, intent.type, text, streams.asSequence().map(Uri::toString),
        items.mapNotNull { it.uri?.toString() })
}
