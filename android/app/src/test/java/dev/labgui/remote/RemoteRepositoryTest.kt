package dev.labgui.remote

import android.app.Application
import android.os.Looper
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.Protocol as HttpProtocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.LooperMode
import java.io.IOException
import java.time.Duration

@RunWith(RobolectricTestRunner::class)
@Config(application = Application::class)
@LooperMode(LooperMode.Mode.PAUSED)
class RemoteRepositoryTest {
    private val sockets = TestSocketFactory()
    private val looper get() = shadowOf(Looper.getMainLooper())
    private lateinit var repository: RemoteRepository
    private val state get() = repository.state.value

    @Before fun connect() {
        val context: Application = RuntimeEnvironment.getApplication()
        repository = RemoteRepository(context, ConnectionSettings(context), sockets)
        repository.setVisible(true)
        assertTrue(repository.connect("ws://server.example", ""))
    }

    @Test fun minimumProtocolAboveTheAppRangeRequiresAnUpdate() {
        val socket = sockets.opened.last()
        socket.open()
        socket.event(
            """{"type":"welcome","protocol":$PROTOCOL_VERSION,"minimum_protocol":${PROTOCOL_VERSION + 1}}""",
            workspace = null,
        )

        assertEquals(ConnectionPhase.Disconnected, state.phase)
        assertTrue(state.updateRequired)
        assertNotNull(state.error)
        assertFalse(repository.wantsConnection)

        assertTrue(repository.connect("ws://server.example", ""))
        finishConnect()
        assertEquals(ConnectionPhase.Connected, state.phase)
        assertFalse(state.updateRequired)
    }

    @Test fun startupErrorCanReportTheServerMinimum() {
        val socket = sockets.opened.last()
        socket.open()
        socket.event(
            """{"type":"error","message":"Upgrade required","minimum_protocol":${PROTOCOL_VERSION + 1}}""",
            workspace = null,
        )

        assertEquals(ConnectionPhase.Disconnected, state.phase)
        assertEquals("Upgrade required", state.error)
        assertTrue(state.updateRequired)
    }

    @Test fun protocolBelowTheSupportedRangeDisconnectsWithoutSuggestingAnUpdate() {
        val socket = sockets.opened.last()
        socket.open()
        socket.event("""{"type":"welcome","protocol":${MIN_PROTOCOL_VERSION - 1}}""", workspace = null)

        assertEquals(ConnectionPhase.Disconnected, state.phase)
        assertFalse(state.updateRequired)
        assertNotNull(state.error)
        assertFalse(repository.wantsConnection)
    }

    @Test fun higherServerVersionNegotiatesDownToTheClientVersion() {
        finishConnect(protocol = PROTOCOL_VERSION + 1)
        assertEquals(ConnectionPhase.Connected, state.phase)
        assertFalse(state.updateRequired)
        assertNull(state.error)
    }

    @Test fun welcomeAtCurrentProtocolConnects() {
        finishConnect()
        assertEquals(ConnectionPhase.Connected, state.phase)
        assertFalse(state.updateRequired)
        assertNull(state.error)
    }

    @Test fun welcomeAtMinimumSupportedProtocolConnects() {
        finishConnect(protocol = MIN_PROTOCOL_VERSION)
        assertEquals(ConnectionPhase.Connected, state.phase)
        assertFalse(state.updateRequired)
    }

    @Test fun approvalQueueIsFetchedOnlyWhenAdvertisedAndRefetchedAfterReconnectAndWorkspaceSwitch() {
        val socket = finishConnect(approvalQueue = true)
        assertCommands(socket, """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":1}}""")
        val original = queuedApprovalFixture("original")
        socket.queueSnapshot(99, original)
        assertTrue(state.approvalDock.pending.isEmpty())
        socket.queueSnapshot(1, original)
        assertEquals(setOf(original.id), state.approvalDock.pending.keys)

        repository.selectThread("s")
        repository.decideQueuedApprovals(ConversationTarget("lab", "s"), listOf(original), true)
        socket.fail()
        assertTrue(state.approvalDock.pending.isEmpty())
        val interrupted = state.approvalDock.decisions.getValue("s")
        assertFalse(interrupted.isSending)
        assertEquals(original, interrupted.failures.single().approval)
        looper.idleFor(Duration.ofSeconds(1))
        val reconnected = finishConnect(approvalQueue = true, replay = false)
        assertCommands(reconnected, """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":3}}""")
        val fresh = queuedApprovalFixture("fresh")
        reconnected.queueSnapshot(3, fresh)
        assertEquals(setOf(fresh.id), state.approvalDock.pending.keys)

        repository.switchWorkspace("new")
        reconnected.event("""{"type":"workspace_selected","id":"new","replay":true}""", workspace = null)
        assertEquals(
            Protocol.json.parseToJsonElement("""{"type":"in_workspace","id":"new","request":{"type":"list_pending_approvals","request":4}}"""),
            Protocol.json.parseToJsonElement(reconnected.sent.last()),
        )
        reconnected.queueSnapshot(3, original)
        assertTrue(state.approvalDock.pending.isEmpty())
        reconnected.queueSnapshot(4, fresh.copy(workspace = "new"), workspace = "new")
        assertEquals("new", state.approvalDock.pending.getValue("fresh").workspace)
    }

    @Test fun failedApprovalSnapshotCanBeRetriedWithANewRequest() {
        val socket = finishConnect(approvalQueue = true)
        socket.event("""{"type":"approval_queue","request":1,"result":{"Err":"queue unavailable"}}""")
        assertEquals("queue unavailable", state.approvalDock.loadError)
        repository.retryApprovalQueue(ConversationTarget("lab", "s"))
        assertCommands(
            socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":1}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":2}}""",
        )
        socket.queueSnapshot(2, queuedApprovalFixture("a"))
        assertNull(state.approvalDock.loadError)
        assertEquals(setOf("a"), state.approvalDock.pending.keys)
    }

    @Test fun legacyServerNeverReceivesApprovalQueueCommands() {
        assertCommands(finishConnect())
    }

    @Test fun bulkApprovalWaitsForTheServerAndReportsACompetingDecision() {
        val socket = finishConnect(approvalQueue = true)
        val a = queuedApprovalFixture("a")
        val b = queuedApprovalFixture("b")
        socket.event("""{"type":"approval_requested","session":"s","request_id":"a","summary":"Shared request"}""")
        socket.queueSnapshot(1, a, b)
        repository.selectThread("s")

        val target = ConversationTarget("lab", "s")
        repository.decideQueuedApprovals(target, listOf(a, b.copy(risk = ApprovalRisk.High)), true)
        assertEquals(2, socket.sent.size)
        repository.decideQueuedApprovals(target, listOf(a, b), true)
        assertCommands(
            socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":1}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"decide_approval","request":2,"approval_id":"a","decision":{"kind":"grant","scope":{"kind":"once"}}}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"decide_approval","request":3,"approval_id":"b","decision":{"kind":"grant","scope":{"kind":"once"}}}}""",
        )
        assertEquals(setOf("a", "b"), state.approvalDock.pending.keys)
        val denied = b.copy(state = ApprovalQueueState.Denied)
        socket.event("""{"type":"approval_queue_changed","approval":${approvalJson(denied)}}""")
        socket.event("""{"type":"approval_decision_result","request":3,"result":{"Err":"already decided"}}""")
        val granted = a.copy(state = ApprovalQueueState.Granted)
        socket.event("""{"type":"approval_decision_result","request":2,"result":{"Ok":${approvalJson(granted)}}}""")
        socket.event("""{"type":"approval_queue_changed","approval":${approvalJson(granted)}}""")

        assertTrue(state.approvalDock.pending.isEmpty())
        assertTrue(state.approvals.isEmpty())
        val decision = state.approvalDock.decisions.getValue("s")
        assertEquals(1, decision.succeeded)
        assertEquals(listOf(ApprovalDecisionFailure(b, "already decided")), decision.failures)
        assertFalse(decision.isSending)
        assertEquals(
            listOf(TimelineRow.ApprovalOutcome(denied, false), TimelineRow.ApprovalOutcome(granted, true)),
            state.timeline("s").rows.values.toList(),
        )
    }

    @Test fun aBulkReviewOnlySendsRequestsThatAreStillPendingAndMatchTheReviewedAction() {
        val socket = finishConnect(approvalQueue = true)
        val a = queuedApprovalFixture("a")
        val b = queuedApprovalFixture("b")
        val expired = queuedApprovalFixture("expired", expiresAt = 1)
        socket.queueSnapshot(1, a, b, expired)
        socket.event("""{"type":"approval_queue_changed","approval":${approvalJson(b.copy(state = ApprovalQueueState.Withdrawn))}}""")
        socket.event("""{"type":"approval_queue_changed","approval":${approvalJson(expired.copy(state = ApprovalQueueState.Expired))}}""")
        repository.selectThread("s")
        repository.decideQueuedApprovals(ConversationTarget("lab", "s"), listOf(a, b, expired), false)
        assertCommands(
            socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"list_pending_approvals","request":1}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"decide_approval","request":2,"approval_id":"a","decision":{"kind":"deny"}}}""",
        )
        assertEquals(listOf(b, expired), state.approvalDock.decisions.getValue("s").failures.map { it.approval })
    }

    @Test fun shellRuleChangesUseTheCurrentWorkspaceAndServerCapability() {
        val socket = finishConnect()
        socket.event("""{"type":"approval_requested","session":"s","request_id":"live","summary":"Bash: pwd","shell_directory":"/work"}""")
        socket.event("""{"type":"worker_approval_policy_changed","policy":"ask"}""")
        repository.rememberShellApproval("live")
        repository.revokeShellApproval("rule")
        assertCommands(socket)
        assertNull(state.shellApprovalRules)

        socket.event("""{"type":"worker_approval_policy_changed","policy":"ask","shell_rules":[]}""", workspace = "other")
        assertNull(state.shellApprovalRules)
        socket.event("""{"type":"worker_approval_policy_changed","policy":"ask","shell_rules":[]}""")
        repository.rememberShellApproval("live")
        repository.rememberShellApproval("live")
        repository.setWorkerApprovalPolicy(WorkerApprovalPolicy.Advisor)
        assertEquals(WorkerApprovalPolicy.Ask, state.workerApprovalPolicy)
        socket.event("""{"type":"approval_resolved","request_id":"live"}""")
        socket.event("""{"type":"worker_approval_policy_changed","policy":"advisor","shell_rules":[{"id":"rule","directory":"/work","command":"pwd"}]}""")
        repository.revokeShellApproval("rule")
        assertEquals(WorkerApprovalPolicy.Advisor, state.workerApprovalPolicy)
        assertCommands(socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"remember_shell_approval","request_id":"live"}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"set_worker_approval_policy","policy":"advisor"}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"revoke_shell_approval","id":"rule"}}""",
        )
    }

    @Test fun onlyTheLiveWorkspaceRequestsAdvertisedPrefixCanBeSaved() {
        val socket = finishConnect()
        socket.event("""{"type":"worker_approval_policy_changed","policy":"ask","shell_rules":[]}""")
        val offered = """{"type":"approval_requested","session":"s","request_id":"family","summary":"Bash: rg foo notes.md","shell_directory":"/work","shell_prefix":"rg"}"""
        socket.event("""{"type":"approval_requested","session":"s","request_id":"exact","summary":"Bash: pwd","shell_directory":"/work"}""")
        socket.event(offered, workspace = "other")
        repository.rememberShellApproval("family", "rg")
        repository.rememberShellApproval("exact", "pwd")
        assertCommands(socket)

        socket.event(offered)
        repository.rememberShellApproval("family", "sh")
        assertCommands(socket)
        repository.rememberShellApproval("family", "rg")
        repository.rememberShellApproval("family", "rg")
        assertEquals(setOf("family"), state.resolving)
        socket.event("""{"type":"approval_resolved","request_id":"family"}""")
        repository.rememberShellApproval("family", "rg")
        socket.event("""{"type":"worker_approval_policy_changed","policy":"ask","shell_rules":[{"id":"family-rule","directory":"/work","command":"rg foo notes.md","prefix":"rg"}]}""")
        assertEquals("rg", state.shellApprovalRules?.single()?.prefix)
        repository.revokeShellApproval("family-rule")
        assertCommands(socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"remember_shell_approval","request_id":"family","prefix":"rg"}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"revoke_shell_approval","id":"family-rule"}}""",
        )
    }

    @Test fun notificationCanOnlyResolveTheLiveEligibleApprovalOnce() {
        val socket = finishConnect()
        socket.event("""{"type":"approval_requested","session":"s","request_id":"live","summary":"Bash: pwd","shell_directory":"/work"}""")
        socket.event("""{"type":"approval_requested","session":"s","request_id":"sensitive","summary":"Bash: git push"}""")
        val target = ConversationTarget("lab", "s")
        val notice = ApprovalNotice(target, "live")
        repository.resolveNotification(notice.copy(target = target.copy(workspace = "other")), accept = true)
        repository.resolveNotification(notice.copy(target = target.copy(session = "other")), accept = true)
        repository.resolveNotification(notice.copy(requestId = "sensitive"), accept = true)
        repository.resolveNotification(notice.copy(requestId = "question"), accept = true)
        assertCommands(socket)

        repository.resolveNotification(notice, accept = false)
        repository.resolveNotification(notice, accept = true)
        socket.event("""{"type":"approval_resolved","request_id":"live"}""")
        repository.resolveNotification(notice, accept = true)
        assertCommands(socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"resolve","request_id":"live","decision":"decline"}}""",
        )
    }

    @Test fun approvalNotificationUsesTheLiveRequestOnlyForTheSelectedWorkspace() {
        val notices = mutableListOf<EventNotice>()
        val subscription = CoroutineScope(Dispatchers.Unconfined).launch {
            repository.notices.collect { notices.add(it) }
        }
        try {
            repository.setServiceRunning(true)
            val socket = finishConnect(
                """{"type":"approval_requested","session":"s","request_id":"old","summary":"Bash: pwd","shell_directory":"/work"}""",
            )
            val notify = """{"type":"notify","session":"s","kind":"approval"}"""
            socket.event(notify)
            assertTrue(notices.isEmpty())

            socket.event("""{"type":"approval_requested","session":"s","request_id":"live","summary":"Bash: pwd","shell_directory":"/work"}""")
            socket.event(notify)
            assertEquals(ApprovalNotice(ConversationTarget("lab", "s"), "live"), notices.single().approval)

            socket.event(notify, workspace = "other")
            assertEquals(2, notices.size)
            assertEquals(ConversationTarget("other", "s"), notices.last().target)
            assertNull(notices.last().approval)
        } finally {
            subscription.cancel()
        }
    }

    @Test fun ignoresUnsupportedEventsAfterWelcomeAndContinuesReceiving() {
        val socket = finishConnect(
            """{"type":"agent_model_capabilities","claude_available":true,"codex_available":true,"front_desk_configured":false}""",
        )
        assertEquals(ConnectionPhase.Connected, state.phase)
        val connected = state
        val ignored = listOf(
            """{"type":"issue_intake_move","session":"s","repo":"owner/repo","from":"old","title":"Issues"}""",
            """{"type":"issue_intake_repos","repos":["owner/repo"]}""",
            """{"type":"chat_capabilities","effort_levels":["high"],"mail_restricted":true}""",
            """{"type":"question_asked","session":"s","request_id":"q","actor":{"kind":"codex"},"questions":[]}""",
            """{"type":"question_resolved","session":"s","request_id":"q","outcome":"dismissed"}""",
            """{"type":"background_commands","threads":{"s":[{"id":"b","summary":"Build","started_at_ms":123,"state":"running"}]}}""",
            """{"type":"notify","session":"s","kind":"future_kind","title":"Thread","extra":true}""",
            """{"type":"future_event","data":{"type":"nested"}}""",
        )
        ignored.forEach { event ->
            socket.event(event)
            assertEquals(event, connected, state)
        }

        socket.event("""{"type":"turn_activity","session":"s","running":true,"extra":{"version":1}}""")
        assertTrue(state.isTurnRunning("s"))
        assertTrue(repository.wantsConnection)
    }

    @Test fun sendsMultipleMessagesDuringATurnAndWaitsForTheirEcho() {
        val socket = finishConnect()
        socket.event("""{"type":"turn_activity","session":"s","running":true}""")

        assertTrue(repository.prompt("s", "first"))
        assertTrue(repository.prompt("s", "second"))
        assertCommands(socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"prompt","session":"s","text":"first"}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"prompt","session":"s","text":"second"}}""",
        )

        socket.event("""{"type":"turn_ended","session":"s","actor":{"kind":"claude"}}""")
        socket.event("""{"type":"user_prompt","session":"s","item":"worker","text":"first","paths":[],"to_worker":true}""")
        assertEquals(listOf("first", "second"), pending())
        socket.event(echo("user:1", "first"))
        assertEquals(listOf("second"), pending())
        socket.event(echo("user:2", "second"))
        assertTrue(pending().isEmpty())
    }

    @Test fun stopsTheTurnAndAnIndividualJobWithSeparateCommands() {
        val socket = finishConnect("""{"type":"turn_activity","session":"s","running":true}""")
        socket.event("""{"type":"chat_waiting","waiting":{"workspace":{"kind":"self_update"},"threads":{}}}""")
        repository.interrupt("s")
        assertEquals(1, socket.sent.size)

        socket.event("""{"type":"turn_activity","session":"s","running":true}""")
        socket.event("""{"type":"tool_call","session":"s","item":"delegate:1","actor":{"kind":"system"},"name":"delegate","input":{"title":"Check the proof","model":"codex","started_at_ms":123}}""")
        repository.interrupt("s")
        repository.stopWorker("s", "delegate:1")
        assertCommands(socket,
            """{"type":"in_workspace","id":"lab","request":{"type":"interrupt","session":"s"}}""",
            """{"type":"in_workspace","id":"lab","request":{"type":"stop_worker","session":"s","job":"delegate:1"}}""",
        )
    }

    @Test fun drainAllowsSendingAndMarksAcknowledgedInputAsHeld() {
        val socket = finishConnect()
        assertTrue(repository.prompt("s", "before drain"))
        socket.event(drain("draining"), workspace = null)
        socket.event(echo("user:before", "before drain"))
        assertFalse(state.hasHeldInput("s"))

        assertTrue(repository.prompt("s", "hold this"))
        assertEquals(listOf("hold this"), pending())
        assertFalse(state.hasHeldInput("s"))

        socket.event(echo("user:1", "hold this"))
        assertTrue(pending().isEmpty())
        assertTrue(state.hasHeldInput("s"))
        assertNull(state.error)

        socket.fail()
        looper.idleFor(Duration.ofSeconds(1))
        val next = finishConnect(echo("user:1", "hold this"), drainProgress = "draining")
        assertTrue(state.hasHeldInput("s"))
        next.event(drain("cancelled"), workspace = null)
        assertFalse(state.hasHeldInput("s"))
    }

    @Test fun reconnectKeepsPendingAndIgnoresPreviouslySeenIdenticalEchoes() {
        val socket = finishConnect(echo("user:old", "same"))
        repository.selectThread("s")
        assertTrue(repository.prompt("s", "same"))
        assertTrue(repository.prompt("s", "same"))
        socket.event(echo("user:first", "same"))
        assertEquals(listOf("same"), pending())

        socket.fail()
        assertEquals(ConnectionPhase.Retrying, state.phase)
        assertEquals("s", state.selection)
        assertEquals(listOf("same"), pending())
        assertFalse(repository.prompt("s", "draft"))

        looper.idleFor(Duration.ofSeconds(1))
        val next = finishConnect(echo("user:old", "same"), echo("user:first", "same"))
        assertEquals(ConnectionPhase.Connected, state.phase)
        assertEquals("s", state.selection)
        assertEquals(listOf("same"), pending())
        assertEquals(1, next.sent.size)

        next.fail()
        looper.idleFor(Duration.ofSeconds(1))
        finishConnect(echo("user:old", "same"), echo("user:first", "same"), echo("user:second", "same"))
        assertTrue(pending().isEmpty())
    }

    @Test fun appliesChatActivityAtThreadSyncBeforeLoadingHistory() {
        val socket = beginScoped()
        socket.event("""{"type":"chat_activity","turns":["s"],"workers":{}}""")
        assertFalse(state.isTurnRunning("s"))

        socket.sync(10)

        assertTrue(state.isTurnRunning("s"))
        assertTrue(socket.historyRequests().isEmpty())
    }

    @Test fun selectedTailAndOlderPagesDeduplicateAndKeepChronologicalOrder() {
        val socket = beginScoped()
        repository.selectThread("s")
        assertTrue(socket.historyRequests().isEmpty())
        socket.sync(10)
        val tail = socket.historyRequests().single()
        assertEquals("get_thread_tail", tail["type"]?.jsonPrimitive?.content)

        socket.event(delta("latest", "tail"), seq = 11)
        socket.event(delta("latest", " continued"), seq = 12)
        socket.tail(1, 11, worker(), delta("latest", "tail"), before = 40, workers = """["delegate:sparse"]""")
        assertEquals("tail continued", message("latest"))
        repository.selectThread(null)
        repository.selectThread("s")
        assertEquals(1, socket.historyRequests().size)

        socket.event("""{"type":"delegate_progress","session":"s","job":"delegate:sparse","summary":"live"}""", seq = 13)
        repository.requestOlderHistory(historyTarget)
        repository.requestOlderHistory(historyTarget)
        assertEquals(2, socket.historyRequests().size)
        assertEquals("40", socket.historyRequests().last()["before"]?.jsonPrimitive?.content)
        assertEquals("7", socket.historyRequests().last()["generation"]?.jsonPrimitive?.content)

        socket.event(delta("older", " new"), seq = 15)
        socket.page(2, 14, delta("older", "old"), worker(), delta("middle", "middle"), before = 20, generation = 6)
        assertEquals(listOf("delegate:sparse", "latest"), rowIds())
        socket.page(2, 14, delta("older", "old"), worker(), delta("middle", "middle"), before = 20, workspace = "wrong")
        assertEquals(listOf("delegate:sparse", "latest"), rowIds())
        socket.page(2, 14, delta("older", "old"), worker(), delta("middle", "middle"), before = 20)
        assertEquals(listOf("older", "delegate:sparse", "middle", "latest"), rowIds())
        assertEquals("old new", message("older"))
        assertEquals("live", (state.timeline("s").rows["delegate:sparse"] as TimelineRow.Job).summary)
        socket.page(2, 14, delta("unexpected", "stale"))
        assertEquals(listOf("older", "delegate:sparse", "middle", "latest"), rowIds())

        repository.requestOlderHistory(historyTarget)
        socket.page(3, 15, """{"type":"future_history_item"}""", before = 1)
        assertEquals("1", socket.historyRequests().last()["before"]?.jsonPrimitive?.content)
        socket.page(4, 15, echo("user:first", "first"))
        assertEquals(listOf("user/user:first", "older", "delegate:sparse", "middle", "latest"), rowIds())
        repository.requestOlderHistory(historyTarget)
        assertEquals(4, socket.historyRequests().size)
        assertNull(state.error)
    }

    @Test fun aTailSnapshotCannotAcknowledgeANewPromptWithMatchingHistoricalText() {
        val socket = beginScoped()
        socket.sync(10)
        repository.selectThread("s")
        assertTrue(repository.prompt("s", "same"))
        socket.tail(1, 10, echo("old", "same"))
        assertEquals(listOf("same"), pending())
        socket.event(echo("new", "same"), seq = 11)
        assertTrue(pending().isEmpty())
        assertEquals(listOf("user/old", "user/new"), rowIds())
    }

    @Test fun reconnectResumesCachedRowsAndRetriesAnInterruptedPageWithANewRequest() {
        val first = beginScoped()
        first.sync(5)
        repository.selectThread("s")
        first.tail(1, 5, delta("latest", "A"), before = 40)
        repository.requestOlderHistory(historyTarget)
        first.page(2, 5, delta("older", "B"), before = 20)
        repository.requestOlderHistory(historyTarget)
        first.event(delta("oldest", " + before reconnect"), seq = 6)
        first.fail()

        looper.idleFor(Duration.ofSeconds(1))
        val resumed = beginScoped(replay = false)
        val hello = Protocol.json.parseToJsonElement(resumed.sent.first()).jsonObject
        assertEquals("6", hello["resume_from"]?.jsonPrimitive?.content)
        assertEquals("epoch", hello["epoch"]?.jsonPrimitive?.content)
        assertEquals(
            Protocol.json.parseToJsonElement("""[{"session":"s","generation":7}]"""),
            hello["cached_sessions"],
        )
        resumed.event(delta("latest", "C"), seq = 7)
        resumed.event(delta("new", "D"), seq = 8)
        assertEquals("A", message("latest"))
        assertTrue(resumed.historyRequests().isEmpty())
        resumed.sync(8)
        assertEquals(listOf("older", "latest"), rowIds())
        assertEquals("AC", message("latest"))
        assertEquals("20", resumed.historyRequests().single()["before"]?.jsonPrimitive?.content)
        assertEquals("4", resumed.historyRequests().single()["request"]?.jsonPrimitive?.content)

        first.page(3, 8, delta("bad", "from previous socket"))
        resumed.page(3, 8, delta("bad", "wrong id"))
        resumed.event(delta("latest", "C"), seq = 7)
        resumed.page(4, 8, delta("oldest", "E + before reconnect"))
        assertEquals(listOf("oldest", "older", "latest", "new"), rowIds())
        assertEquals("AC", message("latest"))
        assertEquals("E + before reconnect", message("oldest"))
        assertEquals("D", message("new"))
    }

    @Test fun reconnectResetReplacesThePreviousGeneration() {
        val first = beginScoped()
        first.sync(20)
        repository.selectThread("s")
        first.tail(1, 20, delta("latest", "before reset"), before = 40)
        first.fail()

        looper.idleFor(Duration.ofSeconds(1))
        val resumed = beginScoped(replay = false)
        resumed.sync(3, reset = """["s"]""")
        assertTrue(rowIds().isEmpty())
        assertEquals("get_thread_tail", resumed.historyRequests().single()["type"]?.jsonPrimitive?.content)
        resumed.tail(2, 3, delta("latest", "after reset"), generation = 8)
        repository.requestOlderHistory(historyTarget)
        assertEquals(1, resumed.historyRequests().size)
        assertEquals("after reset", message("latest"))
    }

    @Test fun oldServerUsesTheEagerTailWithoutPagingOrErrors() {
        val socket = finishConnect(
            """{"type":"thread_history_start","session":"s","before":40,"workers":[]}""",
            delta("latest", "legacy"), protocol = THREAD_HISTORY_PROTOCOL - 1,
        )
        repository.selectThread("s")
        repository.requestOlderHistory(historyTarget)
        assertEquals(listOf("latest"), rowIds())
        assertTrue(socket.historyRequests().isEmpty())
        assertNull(state.error)
    }

    @Test fun protocol62PagesFromTheEagerTailWithNoGeneration() {
        val socket = finishConnect(
            """{"type":"thread_history_start","session":"s","before":40,"workers":[]}""",
            delta("latest", "legacy"), protocol = THREAD_HISTORY_PROTOCOL,
        )
        repository.selectThread("s")
        repository.requestOlderHistory(historyTarget)
        assertCommands(socket, """{"type":"in_workspace","id":"lab","request":{
            "type":"get_thread_history","session":"s","request":1,"before":40
        }}""")
        socket.event("""{"type":"thread_history_page","session":"s","request":1,"events":[${delta("older", "old")}],"next_before":null}""")
        repository.requestOlderHistory(historyTarget)
        assertEquals(1, socket.historyRequests().size)
        assertEquals(listOf("older", "latest"), rowIds())
        assertNull(state.error)
    }

    @Test fun downgradeAfterScopedResumeReconnectsWithNoCursor() {
        val first = beginScoped()
        first.sync(5)
        repository.selectThread("s")
        first.tail(1, 5, delta("latest", "modern"))
        first.fail()
        looper.idleFor(Duration.ofSeconds(1))

        val attempted = sockets.opened.last()
        attempted.open()
        attempted.event("""{"type":"welcome","protocol":61}""", workspace = null)
        assertNotSame(attempted, sockets.opened.last())
        val legacy = finishConnect(delta("new", "legacy"), protocol = 61)
        val hello = Protocol.json.parseToJsonElement(legacy.sent.first()).jsonObject
        assertFalse("resume_from" in hello)
        assertFalse("cached_sessions" in hello)
        repository.requestOlderHistory(historyTarget)
        assertEquals(listOf("new"), rowIds())
        assertTrue(legacy.historyRequests().isEmpty())
        assertNull(state.error)
    }

    private val historyTarget = ConversationTarget("lab", "s")
    private fun rowIds() = state.timeline("s").rows.keys.toList()
    private fun message(id: String) = (state.timeline("s").rows[id] as TimelineRow.Message).text

    private fun beginScoped(replay: Boolean = true): TestSocket {
        val socket = sockets.opened.last()
        socket.open()
        socket.event("""{"type":"welcome","protocol":64,"selection_scoped_transcripts":true}""", workspace = null)
        socket.event("""{"type":"workspace_selected","id":"lab","replay":$replay}""", workspace = null)
        socket.event("""{"type":"threads","threads":[{"id":"s","title":"Thread","updated":0}]}""")
        return socket
    }

    private fun TestSocket.sync(through: Int, reset: String = "[]") =
        event("""{"type":"thread_sync","epoch":"epoch","through":$through,"reset_sessions":$reset}""")

    private fun TestSocket.tail(
        request: Int, through: Int, vararg events: String, before: Int? = null, workers: String = "[]", generation: Int = 7,
    ) = event("""{"type":"thread_tail","session":"s","request":$request,"generation":$generation,"through":$through,
        "workers":$workers,"next_before":$before,"events":[${events.joinToString()}]}""")

    private fun TestSocket.page(
        request: Int, through: Int, vararg events: String, before: Int? = null, generation: Int = 7, workspace: String = "lab",
    ) = event("""{"type":"thread_history_page","session":"s","request":$request,"generation":$generation,"through":$through,
        "next_before":$before,"events":[${events.joinToString()}]}""", workspace)

    private fun TestSocket.historyRequests() = sent.drop(1).mapNotNull {
        Protocol.json.parseToJsonElement(it).jsonObject["request"]?.jsonObject
    }

    private fun delta(item: String, text: String) =
        """{"type":"text_delta","session":"s","item":"$item","actor":{"kind":"codex"},"text":"$text"}"""

    private fun worker() =
        """{"type":"tool_call","session":"s","item":"delegate:sparse","actor":{"kind":"codex"},"name":"delegate","input":{"model":"codex"}}"""

    private fun pending() = state.pendingPrompts("s").map { it.text }

    private fun assertCommands(socket: TestSocket, vararg expected: String) {
        assertEquals(
            expected.map(Protocol.json::parseToJsonElement),
            socket.sent.drop(1).map(Protocol.json::parseToJsonElement),
        )
    }

    private fun finishConnect(
        vararg initial: String,
        drainProgress: String? = null,
        protocol: Int = PROTOCOL_VERSION,
        approvalQueue: Boolean = false,
        replay: Boolean = true,
    ): TestSocket {
        val socket = sockets.opened.last()
        socket.open()
        socket.event("""{"type":"welcome","protocol":$protocol,"approval_queue":$approvalQueue}""", workspace = null)
        if (drainProgress != null) socket.event(drain(drainProgress), workspace = null)
        socket.event("""{"type":"workspace_selected","id":"lab","replay":$replay}""", workspace = null)
        initial.forEach { socket.event(it) }
        socket.event("""{"type":"threads","threads":[{"id":"s","title":"Thread","updated":0}]}""")
        return socket
    }

    private fun approvalJson(approval: QueuedApproval) =
        Protocol.json.encodeToString(QueuedApproval.serializer(), approval)

    private fun TestSocket.queueSnapshot(request: Int, vararg approvals: QueuedApproval, workspace: String = "lab") {
        val items = approvals.joinToString(",") { approvalJson(it) }
        event("""{"type":"approval_queue","request":$request,"result":{"Ok":[$items]}}""", workspace)
    }

    private fun echo(item: String, text: String) =
        """{"type":"text_delta","session":"s","item":"$item","actor":{"kind":"user"},"text":"$text"}"""

    private fun drain(progress: String) =
        """{"type":"drain_status","status":{"started_at":123,"reason":"update","progress":"$progress"}}"""

    private class TestSocketFactory : WebSocket.Factory {
        val opened = mutableListOf<TestSocket>()

        override fun newWebSocket(request: Request, listener: WebSocketListener): WebSocket =
            TestSocket(request, listener).also { opened.add(it) }
    }

    private class TestSocket(private val request: Request, private val listener: WebSocketListener) : WebSocket {
        val sent = mutableListOf<String>()

        fun open() {
            val response = Response.Builder().request(request).protocol(HttpProtocol.HTTP_1_1)
                .code(101).message("Switching Protocols").build()
            listener.onOpen(this, response)
            shadowOf(Looper.getMainLooper()).idle()
        }

        fun event(event: String, workspace: String? = "lab", seq: Int = 0) {
            val scope = workspace?.let { "\"workspace\":\"$it\"," }.orEmpty()
            listener.onMessage(this, """{"seq":$seq,$scope"event":$event}""")
            shadowOf(Looper.getMainLooper()).idle()
        }

        fun fail() {
            listener.onFailure(this, IOException("connection lost"), null)
            shadowOf(Looper.getMainLooper()).idle()
        }

        override fun request(): Request = request
        override fun queueSize(): Long = 0
        override fun send(text: String): Boolean = sent.add(text)
        override fun send(bytes: ByteString): Boolean = error("Unexpected binary message")
        override fun close(code: Int, reason: String?): Boolean = true
        override fun cancel() = Unit
    }
}
