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
    fun namespacedNestedComponentsRemainOneReadableCodeBlock() {
        val xml = """
            <fuzukue:card xmlns:fuzukue="urn:fuzukue:ui:v1" title="Proof status">
              <fuzukue:grid columns="2">
                <fuzukue:stat label="Reviewed" value="10"/>
                <fuzukue:stat label="Open" value="3"/>
              </fuzukue:grid>
              <fuzukue:text>Press <fuzukue:kbd>Enter</fuzukue:kbd>.</fuzukue:text>
            </fuzukue:card>
        """.trimIndent()
        val expected = listOf(Block(xml, BlockStyle.Code))
        assertEquals(expected, markdownBlocks("~~~fuzukue-v1\n$xml"))
        assertEquals(expected, markdownBlocks("~~~fuzukue-v1\n$xml\n~~~\n"))
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
