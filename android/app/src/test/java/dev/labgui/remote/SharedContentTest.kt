package dev.labgui.remote

import org.junit.Assert.*
import org.junit.Test

class SharedContentTest {
    @Test fun plainTextPrefillsAndDoesNotAcceptStreams() {
        val share = parseSharedContent(SHARE_SEND, "text/plain", "Read this", sequenceOf("content://images/a"), emptySequence())
        assertEquals(SharedContent(text = "Read this"), share)
    }

    @Test fun imageShareCanAlsoCarryACaption() {
        val share = parseSharedContent(SHARE_SEND, "image/png", "Screenshot", sequenceOf("content://images/a"), emptySequence())
        assertEquals(SharedContent("Screenshot", listOf("content://images/a")), share)
    }

    @Test fun multipleImagesKeepStreamThenClipOrderWithoutDuplicates() {
        val share = parseSharedContent(
            SHARE_SEND_MULTIPLE, "image/*", "  ",
            sequenceOf("content://images/a", "file:///private", "content://images/b"),
            sequenceOf("content://images/b", "content://images/c"),
        )
        assertEquals(SharedContent(images = listOf("content://images/a", "content://images/b", "content://images/c")), share)
        assertNull(parseSharedContent("VIEW", "image/png", "Other intent", emptySequence(), emptySequence()))
    }

    @Test fun sharePreservesOneOverflowItemSoTheComposerCanShowTheLimit() {
        val images = (1..20).asSequence().map { "content://images/$it" }
        val share = parseSharedContent(SHARE_SEND_MULTIPLE, "image/*", null, images, emptySequence())!!
        assertEquals(MAX_ATTACHMENTS + 1, share.images.size)
        assertEquals(MAX_ATTACHMENTS + 1, share.merge(SharedContent(images = listOf("content://images/21"))).images.size)
    }
}
