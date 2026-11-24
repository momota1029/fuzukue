package dev.labgui.remote

import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream

class AttachmentRulesTest {
    @Test fun chunksFollowAcknowledgementsUpToTheServerLimit() {
        val size = MAX_ATTACHMENT_BYTES
        var cursor = UploadCursor(size)
        for (index in 0 until 99) {
            assertEquals(index.toLong() * ATTACHMENT_CHUNK_BYTES, cursor.offset)
            assertEquals(ATTACHMENT_CHUNK_BYTES, cursor.chunkSize)
            assertFalse(cursor.finalChunk)
            assertNull(cursor.acknowledge(cursor.offset))
            assertNull(cursor.acknowledge(cursor.end + 1))
            cursor = cursor.acknowledge(cursor.end)!!
        }
        assertTrue(cursor.finalChunk)
        assertEquals(size, cursor.end)
        assertNull(cursor.acknowledge(size))
        assertTrue(UploadCursor(0).finalChunk)
        assertEquals(0, UploadCursor(0).chunkSize)
        assertEquals(3, UploadCursor(ATTACHMENT_CHUNK_BYTES + 3L, ATTACHMENT_CHUNK_BYTES.toLong()).chunkSize)
        assertThrows(IllegalArgumentException::class.java) { UploadCursor(size + 1) }
    }

    @Test fun boundedCopyReadsAtMostOneBytePastItsLimit() {
        val source = ByteArrayInputStream(byteArrayOf(1, 2, 3, 4, 5))
        val output = ByteArrayOutputStream()
        assertFalse(copyAttachment(source, output, 3))
        assertArrayEquals(byteArrayOf(1, 2, 3), output.toByteArray())
        assertEquals(1, source.available())
        assertTrue(copyAttachment(ByteArrayInputStream(byteArrayOf(1)), output, 1))
    }

    @Test fun attachmentNamesFitTheServersUtf8ComponentRule() {
        for (name in listOf("../secret", "a/b", "a\\b", "C:secret", ".hidden", "a\u0000", "画".repeat(81))) {
            assertFalse(name, validAttachmentName(name))
            assertTrue(name, validAttachmentName(safeAttachmentName(name)))
        }
        val name = safeAttachmentName("画像📷".repeat(30) + ".jpeg", "jpg")
        assertTrue(validAttachmentName(name))
        assertTrue(name.endsWith(".jpg"))
        assertFalse(name.contains('\uFFFD'))
        assertTrue(validInboxPath("inbox/2026-11-23/$name"))
        assertFalse(validInboxPath("inbox/2026-11-23/../$name"))
    }

    @Test fun scalesOnlyWhenTheByteLimitRequiresIt() {
        assertEquals(AttachmentSizing.Original, attachmentSizing(MAX_ATTACHMENT_BYTES, image = true))
        assertEquals(AttachmentSizing.Original, attachmentSizing(MAX_ATTACHMENT_BYTES, image = false))
        assertEquals(AttachmentSizing.Reencode, attachmentSizing(100, image = true, hasLocation = true))
        assertEquals(AttachmentSizing.Downscale, attachmentSizing(MAX_ATTACHMENT_BYTES + 1, image = true))
        assertEquals(AttachmentSizing.Reject, attachmentSizing(MAX_ATTACHMENT_BYTES + 1, image = false))
        assertEquals(AttachmentSizing.Reject, attachmentSizing(MAX_IMAGE_SOURCE_BYTES + 1, image = true))
        assertEquals(3500 to 1750, fitAttachmentImage(8000, 4000, 3500))
        assertEquals(1 to 120, fitAttachmentImage(1, 30_000, 120))
        assertEquals(50 to 80, fitAttachmentImage(50, 80, 120))
    }

    @Test fun imageMagicMustAgreeWithTheProviderMimeType() {
        val png = attachmentImage(byteArrayOf(-119, 80, 78, 71, 13, 10, 26, 10))
        assertEquals(AttachmentImage.Png, png)
        assertTrue(png!!.matchesMime("image/*"))
        assertTrue(png.matchesMime("application/octet-stream"))
        assertFalse(png.matchesMime("image/jpeg"))
        assertFalse(png.matchesMime("application/pdf"))
        assertNull(attachmentImage("%PDF-1.7".toByteArray()))
        assertNull(attachmentImage("<svg>".toByteArray()))
    }

    @Test fun uploadTransitionsRequireValidProgressAndCompletionAndAllowRetryAndRemoval() {
        val pending = AttachmentStatus.Pending
        val size = ATTACHMENT_CHUNK_BYTES + 45L
        val start = pending.advance(AttachmentEvent.Start(size))!!
        assertEquals(AttachmentStatus.Uploading(UploadCursor(size)), start)
        val failed = AttachmentStatus.Failed(AttachmentProblem.Upload)
        assertEquals(failed, start.advance(AttachmentEvent.Progress(size)))
        assertEquals(failed, start.advance(AttachmentEvent.Uploaded("inbox/2026-11-23/a.png")))
        assertEquals(pending, failed.advance(AttachmentEvent.Retry))
        val disconnected = start.advance(AttachmentEvent.Fail(AttachmentProblem.Disconnected))!!
        assertEquals(pending, disconnected.advance(AttachmentEvent.Retry))
        val last = start.advance(AttachmentEvent.Progress(ATTACHMENT_CHUNK_BYTES.toLong()))!!
        assertEquals(failed, last.advance(AttachmentEvent.Uploaded(null)))
        assertEquals(failed, last.advance(AttachmentEvent.Uploaded("../a.png")))
        val done = last.advance(AttachmentEvent.Uploaded("inbox/2026-11-23/a.png"))!!
        assertEquals(AttachmentStatus.Done("inbox/2026-11-23/a.png"), done)
        assertEquals(done, done.advance(AttachmentEvent.Fail(AttachmentProblem.Disconnected)))
        val invalid = pending.advance(AttachmentEvent.Fail(AttachmentProblem.InvalidImage))!!
        assertEquals(invalid, invalid.advance(AttachmentEvent.Retry))
        listOf(pending, start, done, failed).forEach { assertNull(it.advance(AttachmentEvent.Remove)) }
    }

    @Test fun composerPromotesStopOnlyDuringAnEmptyRunningTurn() {
        assertEquals(ComposerActions(false, false, false), composerActions(false, true, false, false, true))
        assertEquals(ComposerActions(true, false, false), composerActions(true, true, false, false, true))
        assertEquals(ComposerActions(false, true, true), composerActions(true, true, true, false, true))
        assertEquals(ComposerActions(false, true, false), composerActions(true, true, true, true, false))
        assertEquals(ComposerActions(false, true, false), composerActions(true, false, true, false, true))
        assertEquals(ComposerActions(false, false, true), composerActions(false, true, false, true, true))
        assertEquals(ComposerActions(false, false, false), composerActions(false, true, false, true, false))
    }
}
