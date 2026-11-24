package dev.labgui.remote

import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasScrollToIndexAction
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performScrollToIndex
import androidx.compose.ui.test.performScrollToKey
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeDown
import androidx.compose.ui.text.TextLayoutResult
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import org.robolectric.annotation.LooperMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@LooperMode(LooperMode.Mode.PAUSED)
@Config(qualifiers = "ja-rJP-w412dp-h870dp-mdpi")
class TimelineScrollTest {
    @get:Rule val compose = createComposeRule()

    @Test fun scrollingToTheTopRequestsHistoryAndKeepsTheAnchorAfterPrepend() {
        val original = Timeline(rows = buildMap {
            put("sparse", TimelineRow.Note("Anchor row"))
            repeat(70) { put("row-$it", TimelineRow.Note("Transcript row $it")) }
        })
        val state = mutableStateOf(RemoteState(
            phase = ConnectionPhase.Connected,
            workspace = "lab",
            selection = "thread",
            threads = listOf(ThreadInfo("thread", "History", 100)),
            timelines = mapOf("thread" to original),
        ))
        val requests = mutableListOf<ConversationTarget>()
        compose.setContent { RemoteTheme(dark = false) { RemoteApp(state.value, onOlderHistory = { requests.add(it) }) } }
        val list = compose.onNode(hasScrollToIndexAction())
        compose.runOnIdle { assertTrue(requests.isEmpty()) }
        list.performScrollToKey("event/sparse")
        list.performTouchInput { swipeDown() }
        compose.runOnIdle { assertTrue(ConversationTarget("lab", "thread") in requests) }

        val anchor = compose.onNodeWithText("Anchor row").assertIsDisplayed()
        val before = anchor.fetchSemanticsNode().boundsInRoot.top
        val extra = Timeline(rows = linkedMapOf(
            "first" to TimelineRow.Note("Older row"),
            "sparse" to TimelineRow.Note("Page copy of anchor"),
            "between" to TimelineRow.Note("Between anchor and original tail"),
        ))
        val merged = original.prependHistory(extra, listOf("sparse")).copy(olderPages = 1)
        compose.runOnIdle { state.value = state.value.copy(timelines = mapOf("thread" to merged)) }
        anchor.assertIsDisplayed()
        assertEquals(before, anchor.fetchSemanticsNode().boundsInRoot.top, 1f)
        compose.onRoot().captureRoboImage("build/shots/timeline-older-page.png")
        list.performScrollToKey("event/first")
        compose.onNodeWithText("Older row").assertIsDisplayed()
    }

    @Test fun japaneseReplyKeepsEveryParagraphAfterScrollingAway() {
        val paragraphs = listOf(
            "まず、会話の最初に戻り、どの条件を使ったかを読み直します。" +
                "前の段落に出てくる定義も、一つずつ照らし合わせます。",
            "次に、結論までの説明を整理します。後から読み返しても途中の計算を追えるように、" +
                "式を変形した理由と、使った仮定の名前を書き添えます。",
            "最後に、会話をいったん画面の外に送り、戻ってきたときに冒頭から読めるか確かめます。",
        )
        assertReplySurvivesScroll(paragraphs.joinToString("\n\n"), paragraphs, "timeline-japanese-return")
    }

    @Test fun markdownReplyKeepsCodeListAndMathAfterScrollingAway() {
        val intro = "計算の流れを短くまとめます。各段階で何を使ったかが分かるように、" +
            "確認する順番と、そのとき参照する式を下に並べます。"
        val code = "確認には `git diff` を使います。"
        val math = "数式は \$x^2 + 1\$ と書きます。"
        val closing = "以上が今回の変更点です。会話を読み返したときも、最後まで続けて読めることを確認します。"
        assertReplySurvivesScroll(
            "$intro\n\n- $code\n- $math\n\n$closing",
            listOf(intro, "• ${code.replace("`", "")}\n• ${math.replace("\$", "")}", closing),
            "timeline-markdown-return",
        )
    }

    private fun assertReplySurvivesScroll(source: String, paragraphs: List<String>, shot: String) {
        val rows = buildMap {
            repeat(80) { put("older-$it", TimelineRow.Note("以前の記録 $it")) }
            put("reply", TimelineRow.Message(source, user = false))
            repeat(8) { put("newer-$it", TimelineRow.Note("新しい記録 $it")) }
        }
        val state = RemoteState(
            phase = ConnectionPhase.Connected,
            workspace = "lab",
            selection = "thread",
            threads = listOf(ThreadInfo("thread", "会話の読み直し", 100)),
            timelines = mapOf("thread" to Timeline(rows = rows)),
        )
        compose.setContent { RemoteTheme(dark = false) { RemoteApp(state) } }
        val timeline = compose.onNode(hasScrollToIndexAction())
        timeline.performScrollToKey("event/reply")
        val before = paragraphs.map(::visibleLayout)
        assertTrue("The opening paragraph must wrap", before.first().lineCount > 1)

        timeline.performScrollToIndex(40)
        compose.onNodeWithText(paragraphs.first()).assertDoesNotExist()
        timeline.performScrollToKey("event/reply")

        val after = paragraphs.map(::visibleLayout)
        assertEquals(before.map { it.size }, after.map { it.size })
        assertEquals(before.map { it.lineCount }, after.map { it.lineCount })
        compose.onRoot().captureRoboImage("build/shots/$shot.png")
    }

    private fun visibleLayout(paragraph: String): TextLayoutResult {
        val node = compose.onNodeWithText(paragraph, useUnmergedTree = true).assertIsDisplayed()
        val results = mutableListOf<TextLayoutResult>()
        node.performSemanticsAction(SemanticsActions.GetTextLayoutResult) { it(results) }
        val layout = results.single()
        assertEquals(paragraph, layout.layoutInput.text.text)
        assertFalse(layout.hasVisualOverflow)
        assertEquals(paragraph.length, layout.getLineEnd(layout.lineCount - 1))
        val bounds = node.fetchSemanticsNode().boundsInRoot
        assertEquals(layout.size.width.toFloat(), bounds.width, 1f)
        assertEquals(layout.size.height.toFloat(), bounds.height, 1f)
        return layout
    }
}
