package dev.labgui.remote

import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Test

class AttachmentProtocolTest {
    @Test fun uploadChunkAndCancelMatchRustClientMessages() {
        assertMessage(
            ClientMessage.UploadAttachment(4, "screen.png", 3, 1, "YmM=", "s"),
            """{"type":"upload_attachment","id":4,"name":"screen.png","size":3,"offset":1,"data":"YmM=","session":"s"}""",
        )
        assertMessage(ClientMessage.CancelAttachment(4), """{"type":"cancel_attachment","id":4}""")
    }

    @Test fun usesInboxReferencesEvenWhenThePromptHasNoText() {
        val paths = listOf("inbox/2026-11-23/screen.png", "inbox/2026-11-23/notes.txt")
        assertMessage(
            ClientMessage.InWorkspace("lab", composerPrompt("s", "", paths)),
            """{"type":"in_workspace","id":"lab","request":{"type":"prompt_with_attachments","session":"s","text":"","paths":["inbox/2026-11-23/screen.png","inbox/2026-11-23/notes.txt"]}}""",
        )
        assertMessage(composerPrompt("s", "text", emptyList()), """{"type":"prompt","session":"s","text":"text"}""")
    }

    @Test fun decodesAcknowledgementsAndTheEchoWithoutChangingReferences() {
        assertEquals(ServerEvent.AttachmentProgress(4, 1), decode("""{"type":"attachment_progress","id":4,"offset":1}"""))
        val path = "inbox/2026-11-23/screen.png"
        assertEquals(ServerEvent.AttachmentUploaded(4, path),
            decode("""{"type":"attachment_uploaded","id":4,"path":"$path","error":null,"proposal":{"to":"inbox"}}"""))
        assertEquals(ServerEvent.AttachmentUploaded(4, error = "rejected"),
            decode("""{"type":"attachment_uploaded","id":4,"path":null,"error":"rejected"}"""))
        val echo = decode("""{"type":"user_prompt","session":"s","item":"u1","text":"","paths":["$path"]}""")
        assertEquals(TimelineRow.Message("", true, "u1", listOf(path)),
            Timeline().reduce(echo as ServerEvent.UserPrompt).rows.getValue("user/u1"))
    }

    private fun assertMessage(message: ClientMessage, expected: String) {
        assertEquals(Json.parseToJsonElement(expected), Json.parseToJsonElement(Protocol.encode(message)))
    }

    private fun decode(event: String) = Protocol.decode("""{"seq":1,"workspace":"lab","event":$event}""").event
}
