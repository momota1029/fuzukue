package dev.labgui.remote

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AssistantSilenceTest {
    @Test fun blankAndSentinelRowsStayInHistoryAfterCompletionAndReplay() {
        for (text in listOf("<silent/>", " \n<silent/>\t\n ", "", " \t\n")) {
            val timeline = Timeline().reply(text, replaying = true).endTurn()

            assertTrue(timeline.visibleRows.isEmpty())
            assertEquals(TimelineRow.Message(text, user = false), timeline.rows["reply"])
        }
    }

    @Test fun streamingPrefixesAndTheCompletedSentinelNeverCreateVisibleRows() {
        var timeline = Timeline().reply(" \n")
        for (character in "<silent/>") {
            timeline = timeline.reply(character.toString())
            assertTrue(timeline.visibleRows.isEmpty())
        }
        assertTrue(timeline.endTurn().visibleRows.isEmpty())
        assertEquals(listOf("reply"), timeline.rows.keys.toList())
    }

    @Test fun aPrefixBecomesVisibleWhenMoreTextArrivesOrTheTurnEnds() {
        val partial = Timeline().reply("<sil")
        assertTrue(partial.visibleRows.isEmpty())
        assertEquals(TimelineRow.Message("<silent/> is the marker", false),
            partial.reply("ent/> is the marker").visibleRows["reply"])

        val next = partial.endTurn().reply("<sil", item = "next")
        assertEquals(listOf("reply"), next.visibleRows.keys.toList())
    }

    @Test fun ordinaryRepliesAndUserSentinelsStayVisible() {
        for (text in listOf("silent mode", "The marker is <silent/>.", "Nothing to report.", "<SILENT/>")) {
            val timeline = Timeline().reply(text)
            assertEquals(timeline.rows, timeline.visibleRows)
        }

        val timeline = Timeline().reduce(ServerEvent.UserPrompt("session", "user", "<silent/>"))
        assertEquals(timeline.rows, timeline.visibleRows)
    }

    private fun Timeline.reply(text: String, item: String = "reply", replaying: Boolean = false) =
        reduce(ServerEvent.TextDelta("session", item, Actor("claude"), text), replaying = replaying)

    private fun Timeline.endTurn() = reduce(ServerEvent.TurnEnded("session", Actor("claude")))
}
