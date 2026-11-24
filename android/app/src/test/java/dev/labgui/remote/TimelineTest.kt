package dev.labgui.remote

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.TimeUnit.MINUTES

class TimelineTest {
    @Test fun composerNoticesReplaceOneStatusWithoutAddingRows() {
        val waiting = """{"type":"local_notice","session":"s","item":"composer-status","text":"Wait for Claude"}"""
        val stopped = """{"type":"local_notice","session":"s","item":"composer-status","text":"Codex stopped"}"""
        val timeline = reduce(waiting, waiting, waiting)
        assertEquals("Wait for Claude", timeline.composerNotice)
        assertTrue(timeline.visibleRows.isEmpty())

        val stop = Protocol.decode("""{"seq":0,"event":$stopped}""").event as ServerEvent.ComposerNotice
        assertSame(timeline, timeline.reduce(stop, replaying = true))
        val updated = timeline.reduce(stop)
        assertEquals("Codex stopped", updated.composerNotice)
        assertTrue(updated.visibleRows.isEmpty())
        assertNull(updated.reduce(ServerEvent.TurnActivity("s", true)).composerNotice)
        assertNull(timeline.reduce(ServerEvent.TurnEnded("s", Actor("claude"))).composerNotice)

        val divider = Protocol.decode(
            """{"seq":0,"event":{"type":"local_notice","session":"s","item":"clear","text":"New session"}}""",
        ).event
        assertEquals(ServerEvent.Unknown("local_notice"), divider)
    }

    @Test fun messagesJoinByItemAndWholeUserMessagesKeepTheirOrder() {
        val timeline = reduce(
            """{"type":"user_prompt","session":"s","item":"user:1","text":"見て","paths":["notes.md"]}""",
            """{"type":"text_delta","session":"s","item":"reply","actor":{"kind":"claude"},"text":"途中"}""",
            """{"type":"text_delta","session":"s","item":"thread-plan:42","actor":{"kind":"system"},"text":"方針を更新: 次は原稿を確認"}""",
            """{"type":"text_delta","session":"s","item":"reply","actor":{"kind":"claude"},"text":"まで確認した"}""",
            """{"type":"text_delta","session":"s","item":"user:1","actor":{"kind":"user"},"text":"続きを見て"}""",
        )
        assertEquals(listOf(
            TimelineRow.Message("見て", true, userItem = "user:1", paths = listOf("notes.md")),
            TimelineRow.Message("途中まで確認した", false),
            TimelineRow.Note("方針を更新: 次は原稿を確認"),
            TimelineRow.Message("続きを見て", true, userItem = "user:1"),
        ), timeline.rows.values.toList())
    }

    @Test fun workerUpdatesOneCardAndDoesNotEndThePartnerTurn() {
        val running = reduce(
            """{"type":"turn_activity","session":"s","running":true}""",
            """{"type":"tool_call","session":"s","item":"delegate:2","actor":{"kind":"system"},"name":"delegate","input":{"agent":"数式チェック","model":"gpt-6.2-sol","started_at_ms":null}}""",
            """{"type":"tool_call","session":"s","item":"delegate:2","actor":{"kind":"system"},"name":"delegate","input":{"agent":"数式チェック","model":"gpt-6.2-sol","started_at_ms":1700000000000}}""",
            """{"type":"delegate_progress","session":"s","job":"delegate:2","summary":"原稿を読んでいる"}""",
            """{"type":"delegate_progress","session":"s","job":"delegate:2","summary":"仮定を確認中。"}""",
            """{"type":"text_delta","session":"s","item":"child","actor":{"kind":"codex","thread_id":"codex","parent_tool_use":"delegate:2"},"text":"内部メモ"}""",
            """{"type":"turn_ended","session":"s","actor":{"kind":"codex","thread_id":"codex","parent_tool_use":"delegate:2"}}""",
        )
        assertTrue(running.running)
        assertEquals(listOf("delegate:2"), running.rows.keys.toList())
        val job = running.rows.values.single() as TimelineRow.Job
        assertEquals("数式チェック", job.info.agent)
        assertEquals(1700000000000L, job.info.startedAt)
        assertEquals("仮定を確認中。", job.summary)
        assertNull(job.ok)

        val finished = running.reduce(decodeEvent(
            """{"type":"tool_result","session":"s","item":"delegate:2","ok":true,"output":"{\"elapsed_ms\":9100,\"text\":\"確認済み\"}"}""",
        ) as ServerEvent.InThread)
        assertEquals(job.copy(ok = true, report = JobReport(9100, "確認済み")), finished.rows.values.single())
        assertFalse(finished.reduce(ServerEvent.TurnEnded("s", Actor("claude"))).running)
    }

    @Test fun planAttentionUsesDistinctBlockerReasonsAndDoesNotRefreshOnSnapshots() {
        val expiry = MINUTES.toNanos(30)
        val blocked = decodeEvent(
            """{"type":"plan_changed","session":"s","plan":{"goal":"Check the reference","status":"waiting_for_user","question":"Which edition?","next":[{"task":"Confirm the edition","blocker":{"reason":"Choose the edition","needs":"decision"}}]}}""",
        ) as ServerEvent.PlanChanged
        assertEquals("Check the reference", blocked.plan.goal)
        assertEquals(PlanBlocker("Choose the edition"), blocked.plan.next.single().blocker)
        val prompt = ServerEvent.UserPrompt("s", "user:2", "Use the new edition")
        val fresh = Timeline().reduce(blocked, attentionNow = 0)
        assertTrue(fresh.planAttention.isVisible(0))
        assertTrue(fresh.reduce(prompt, replaying = true, attentionNow = 1).planAttention.isVisible(expiry))

        val renamedStep = blocked.plan.next.single().copy(task = "Pick an edition", blocker = PlanBlocker(" Choose the edition "))
        val repeated = blocked.copy(plan = blocked.plan.copy(next = blocked.plan.next + renamedStep))
        val acknowledged = fresh.reduce(prompt, attentionNow = 1).reduce(repeated, attentionNow = 2)
        assertEquals(expiry, acknowledged.planAttention.expiresAt)
        assertTrue(acknowledged.planAttention.isVisible(expiry - 1))
        assertFalse(acknowledged.planAttention.isVisible(expiry))

        val newBlocker = PlanStep("Verify on Android", PlanBlocker("Check the device"))
        val changed = repeated.copy(plan = repeated.plan.copy(status = "active", next = repeated.plan.next + newBlocker))
        val updated = acknowledged.reduce(changed, attentionNow = expiry)
        assertTrue(updated.planAttention.isVisible(expiry))
        assertNull(updated.planAttention.expiresAt)
        val removed = updated.reduce(repeated, attentionNow = expiry + 1)
        assertFalse(removed.planAttention.isVisible(expiry + 1))
        assertTrue(removed.reduce(changed, attentionNow = expiry + 2).planAttention.isVisible(expiry + 2))
    }

    @Test fun routineQuestionOnlyAndCompletedPlansDoNotAskForAttention() {
        val routine = Plan("active", listOf(PlanStep("Read the draft"), PlanStep("Review", PlanBlocker("  "))))
        val questionOnly = routine.copy(status = "waiting_for_user", question = "Which edition?")
        val blocked = questionOnly.copy(next = listOf(PlanStep("Pick an edition", PlanBlocker("Choose the edition"))))
        val completed = blocked.copy(status = "done")
        val timeline = Timeline().reduce(ServerEvent.PlanChanged("s", blocked), attentionNow = 0)
        for (plan in listOf(routine, questionOnly, completed)) {
            val updated = timeline.reduce(ServerEvent.PlanChanged("s", plan), attentionNow = 1)
            assertFalse(updated.planAttention.isVisible(1))
        }
    }

    private fun reduce(vararg events: String) =
        events.fold(Timeline()) { state, event -> state.reduce(decodeEvent(event) as ServerEvent.InThread) }
}
