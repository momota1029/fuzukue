package dev.labgui.remote

import android.graphics.Paint
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import io.ratex.RaTeXSpan
import kotlinx.coroutines.CancellationException

private enum class BlockStyle { Paragraph, Heading, Quote, Code, Formula }
private data class Block(val text: String, val style: BlockStyle, val size: Float = 1f)
private data class Formula(val source: String, val display: Boolean) {
    val contentId get() = if (display) "display/$source" else "inline/$source"
}

private class FormulaImage(private val span: RaTeXSpan) {
    private val paint = Paint()
    private val metrics = Paint.FontMetricsInt()
    val width = span.getSize(paint, null, 0, 0, metrics)
    private val baseline = -metrics.ascent
    private val height = (baseline + metrics.descent).coerceAtLeast(1)

    fun inlineContent(density: Density): InlineTextContent = with(density) {
        val placeholder = Placeholder(width.toSp(), height.toSp(), PlaceholderVerticalAlign.TextCenter)
        InlineTextContent(placeholder) {
            Canvas(Modifier.fillMaxSize()) {
                drawIntoCanvas { canvas ->
                    span.draw(canvas.nativeCanvas, null, 0, 0, 0f, 0, baseline, height, paint)
                }
            }
        }
    }
}

private val fencedBlock = Regex(
    """(?ms)^ {0,3}(`{3,}|~{3,})[^\n]*\n(.*?)(?:^ {0,3}\1[ \t]*(?=\n|${'$'})|\z)""",
)
private val displayMath = Regex("""(?s)\${'$'}\${'$'}(.+?)\${'$'}\${'$'}|\\\[(.+?)\\\]""")
private val inline = Regex(
    """(?<!\\)(`+)(.+?)\1|(?<![\\${'$'}])\${'$'}([^\s${'$'}\n](?:[^\n${'$'}]*?[^\s${'$'}\n])?)\${'$'}(?![\d${'$'}])|\\\((.+?)\\\)|\*\*(.+?)\*\*|__(.+?)__|(?<!\w)\*(.+?)\*|(?<!\w)_(.+?)_|~~(.+?)~~|\[([^\]\n]+)]\(([^)\n]+)\)|\\([\\`*{}_\[\]()#+.!~\${'$'}-])""",
)
private val headings = Regex("""^(#{1,6})\s+(.+?)(?:\s+#+)?${'$'}""")
private val bullets = Regex("""^(\s*)[-*+]\s+""")

@Composable
internal fun Markdown(source: String) {
    val blocks = remember(source) { markdownBlocks(source) }
    val context = LocalContext.current
    val density = LocalDensity.current
    val foreground = LocalContentColor.current
    val accent = MaterialTheme.colorScheme.primary
    val chip = MaterialTheme.colorScheme.surfaceContainerHigh
    val body = MaterialTheme.typography.bodyMedium
    val images = remember(context, density, foreground, body.fontSize) {
        mutableStateMapOf<Formula, FormulaImage?>()
    }
    val formulas = remember(blocks) {
        blocks.flatMap { block ->
            when (block.style) {
                BlockStyle.Formula -> listOf(Formula(block.text, true))
                BlockStyle.Code -> emptyList()
                else -> inline.findAll(block.text).mapNotNull { it.inlineFormula() }.toList()
            }
        }.distinct()
    }
    LaunchedEffect(formulas, images) {
        images.keys.retainAll(formulas.toSet())
        for (formula in formulas) {
            if (formula in images || formula.source.length !in 1..1_000) continue
            val fontSize = body.fontSize * (if (formula.display) 1.15f else 1f)
            val sizeDp = with(density) { fontSize.toDp().value }
            images[formula] = try {
                FormulaImage(RaTeXSpan.create(context, formula.source, sizeDp, foreground.toArgb(), formula.display))
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: LinkageError) {
                null
            } catch (_: Exception) {
                null
            }
        }
    }
    BoxWithConstraints {
        val width = constraints.maxWidth
        val rendered = images.toMap()
        val inlineContent = remember(rendered, width, density) {
            buildMap {
                for ((formula, image) in rendered) {
                    if (image != null && image.width <= width) put(formula.contentId, image.inlineContent(density))
                }
            }
        }
        SelectionContainer {
            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                for (block in blocks) {
                    val styled = remember(block, inlineContent.keys, accent, chip) {
                        styledText(block, inlineContent.keys, accent, chip)
                    }
                    val display = block.style == BlockStyle.Formula
                    Text(
                        text = styled,
                        modifier = if (display) Modifier.fillMaxWidth() else Modifier,
                        inlineContent = inlineContent,
                        textAlign = if (display) TextAlign.Center else TextAlign.Start,
                        style = body.copy(fontSize = body.fontSize * block.size, lineHeight = TextUnit.Unspecified),
                    )
                }
            }
        }
    }
}

private fun markdownBlocks(source: String): List<Block> = buildList {
    fun plainText(text: String) {
        text.trim('\n').split(Regex("""\n\s*\n""")).forEach { paragraph ->
            val lines = mutableListOf<String>()
            fun flush() {
                if (lines.isNotEmpty()) add(Block(lines.joinToString("\n"), BlockStyle.Paragraph))
                lines.clear()
            }
            for (line in paragraph.lines()) {
                val heading = headings.matchEntire(line)
                when {
                    heading != null -> {
                        flush()
                        add(Block(heading.groupValues[2], BlockStyle.Heading,
                            if (heading.groupValues[1].length <= 2) 1.25f else 1.1f))
                    }
                    line.startsWith(">") -> {
                        flush()
                        add(Block("│  " + line.removePrefix(">").trimStart(), BlockStyle.Quote))
                    }
                    line.isNotBlank() -> lines += line.replaceFirst(bullets, "${'$'}1• ")
                }
            }
            flush()
        }
    }

    fun textAndMath(text: String) {
        var end = 0
        // Inline code can contain literal dollar pairs.
        val code = Regex("""(`+)[^`\n]+?\1""").findAll(text).map { it.range }.toList()
        for (math in displayMath.findAll(text)) {
            if (code.any { math.range.first in it || math.range.last in it }) continue
            plainText(text.substring(end, math.range.first))
            add(Block((math.groups[1] ?: math.groups[2])!!.value.trim(), BlockStyle.Formula))
            end = math.range.last + 1
        }
        plainText(text.substring(end))
    }

    var end = 0
    for (fence in fencedBlock.findAll(source)) {
        textAndMath(source.substring(end, fence.range.first))
        add(Block(fence.groupValues[2].trimEnd('\n'), BlockStyle.Code))
        end = fence.range.last + 1
    }
    textAndMath(source.substring(end))
}

private fun MatchResult.inlineFormula(): Formula? =
    (groups[3] ?: groups[4])?.let { Formula(it.value, false) }

private fun styledText(block: Block, rendered: Set<String>, accent: Color, chip: Color) = buildAnnotatedString {
    val code = SpanStyle(fontFamily = FontFamily.Monospace, background = chip)
    fun math(formula: Formula) {
        if (formula.contentId in rendered) {
            appendInlineContent(formula.contentId, formula.source.replace('\n', ' '))
        } else {
            withStyle(code) { append(formula.source) }
        }
    }

    val bold = SpanStyle(fontWeight = FontWeight.Bold)
    val italic = SpanStyle(fontStyle = FontStyle.Italic)
    val strike = SpanStyle(textDecoration = TextDecoration.LineThrough)
    val link = SpanStyle(color = accent, textDecoration = TextDecoration.Underline)
    when (block.style) {
        BlockStyle.Code -> withStyle(code) { append(block.text) }
        BlockStyle.Formula -> math(Formula(block.text, true))
        else -> withStyle(when (block.style) {
            BlockStyle.Heading -> bold
            BlockStyle.Quote -> italic
            else -> SpanStyle()
        }) {
            var end = 0
            for (token in inline.findAll(block.text)) {
                append(block.text.substring(end, token.range.first))
                val formula = token.inlineFormula()
                val groups = token.groups
                when {
                    groups[2] != null -> withStyle(code) { append(groups[2]!!.value) }
                    formula != null -> math(formula)
                    groups[5] != null || groups[6] != null -> withStyle(bold) { append((groups[5] ?: groups[6])!!.value) }
                    groups[7] != null || groups[8] != null -> withStyle(italic) { append((groups[7] ?: groups[8])!!.value) }
                    groups[9] != null -> withStyle(strike) { append(groups[9]!!.value) }
                    groups[10] != null -> {
                        withStyle(link) { append(groups[10]!!.value) }
                        withStyle(SpanStyle(fontSize = 0.85.em)) { append(" (${groups[11]!!.value})") }
                    }
                    groups[12] != null -> append(groups[12]!!.value)
                }
                end = token.range.last + 1
            }
            append(block.text.substring(end))
        }
    }
}
