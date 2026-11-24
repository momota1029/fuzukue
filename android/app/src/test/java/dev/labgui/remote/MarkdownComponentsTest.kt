package dev.labgui.remote

import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class MarkdownComponentsTest {
    @Test
    fun helloAdvertisesNoComponents() {
        val hello = Protocol.json.parseToJsonElement(Protocol.encode(ClientMessage.Hello())).jsonObject
        assertFalse("markdown_components" in hello)
    }

    @Test
    fun componentFencesRenderTheirContentsLiterally() {
        val xml = """<callout severity="warn" title="Example">**literal**</callout>"""
        assertEquals(
            listOf(
                Block("Before", BlockStyle.Paragraph),
                Block(xml, BlockStyle.Code),
                Block("After", BlockStyle.Paragraph),
            ),
            markdownBlocks("Before\n\n~~~fuzukue-v1\n$xml\n~~~\n\nAfter"),
        )
    }

    @Test
    fun unknownVersionsAndPartialComponentsStayLiteral() {
        val xml = """<future href="javascript:example()"><script>""" + "\n" + "x".repeat(20_000)
        for (marker in listOf("fuzukue-v1", "fuzukue-v42")) {
            assertEquals(listOf(Block(xml, BlockStyle.Code)), markdownBlocks("~~~$marker\n$xml"))
        }
        val tag = "<future/>"
        assertEquals(listOf(Block(tag, BlockStyle.Paragraph)), markdownBlocks(tag))
    }
}
