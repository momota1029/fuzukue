package dev.labgui.remote

import kotlinx.serialization.json.Json
import org.junit.Assert.*
import org.junit.Test

class ProtocolTest {
    @Test fun folderAccessUsesApprovalAndUnknownSnapshotsAreIgnored() {
        val request = decodeEvent(
            """{"type":"approval_requested","session":"s","request_id":"folder-access-1","summary":"Read-only access"}""",
        ) as ServerEvent.ApprovalRequested
        assertTrue(request.isFolderAccess)
        assertMessage(
            """{"type":"decide_folder_access","request_id":"folder-access-1","expiry":"one_hour"}""",
            ClientMessage.DecideFolderAccess(request.requestId, "one_hour"),
        )
        assertMessage(
            """{"type":"decide_folder_access","request_id":"folder-access-1"}""",
            ClientMessage.DecideFolderAccess(request.requestId, null),
        )
        assertEquals(ServerEvent.Unknown("folder_grants"), decodeEvent(
            """{"type":"folder_grants","session":"s","grants":[]}""",
        ))
        assertEquals(ServerEvent.Unknown("folder_grants_changed"), decodeEvent(
            """{"type":"folder_grants_changed","session":"s"}""",
        ))
    }

    @Test fun handshakeAndWorkspaceCommands() {
        assertMessage("""{"type":"hello","protocol":65,"workspace":"lab","locale":"en","platform":"android","thread_history":true,"selection_scoped_transcripts":true}""", ClientMessage.Hello(workspace = "lab", locale = "en"))
        assertEquals(ServerEvent.Welcome(MIN_PROTOCOL_VERSION), Protocol.decode(
            """{"seq":0,"event":{"type":"welcome","protocol":$MIN_PROTOCOL_VERSION,"server":"secretaire"}}""",
        ).event)
        assertMessage(
            """{"type":"in_workspace","id":"lab","request":{"type":"resolve","request_id":"codex:3","decision":"decline"}}""",
            ClientMessage.InWorkspace("lab", ClientMessage.Resolve("codex:3", "decline")),
        )
    }

    @Test fun decodesNotificationRoutingAndTerminalUpdateStatus() {
        val notice = Protocol.decode(
            """{"seq":0,"workspace":"lab","event":{"type":"notify","session":"s","kind":"question","future_field":true}}""",
        )
        assertEquals("lab", notice.workspace)
        assertEquals(ServerEvent.Notify("s", "question"), notice.event)
        assertEquals(ServerEvent.UpdateChanged(ServerUpdateStatus(false, steps = listOf(ServerUpdateStep("swap", "passed")))),
            decodeEvent("""{"type":"self_update_status","status":{"active":false,"steps":[{"kind":"swap","state":"passed","log":"details"}]}}"""))
    }

    @Test fun savedShellRulesExtendTheExistingApprovalProtocol() {
        assertEquals(ServerEvent.WorkerApprovalPolicyChanged(WorkerApprovalPolicy.Ask),
            decodeEvent("""{"type":"worker_approval_policy_changed","policy":"ask"}"""))
        val rule = ShellApprovalRule("rule", "/work", "pwd")
        assertEquals(ServerEvent.WorkerApprovalPolicyChanged(WorkerApprovalPolicy.Advisor, listOf(rule)),
            decodeEvent("""{"type":"worker_approval_policy_changed","policy":"advisor","shell_rules":[{"id":"rule","directory":"/work","command":"pwd"}]}"""))
        val approval = decodeEvent(
            """{"type":"approval_requested","session":"s","request_id":"live","summary":"Bash: pwd","shell_directory":"/work"}""",
        ) as ServerEvent.ApprovalRequested
        assertEquals("Bash: pwd\n/work", approval.notificationDetails())
        assertNull(approval.shellPrefix)
        assertNull(approval.copy(shellDirectory = null).notificationDetails())
        assertNull(approval.copy(summary = "Bash: " + "x".repeat(101)).notificationDetails())
        assertMessage("""{"type":"remember_shell_approval","request_id":"live"}""", ClientMessage.RememberShellApproval("live"))
        assertMessage("""{"type":"revoke_shell_approval","id":"rule"}""", ClientMessage.RevokeShellApproval("rule"))
        assertMessage("""{"type":"set_worker_approval_policy","policy":"auto"}""", ClientMessage.SetWorkerApprovalPolicy(WorkerApprovalPolicy.Auto))
    }

    @Test fun prefixApprovalFieldsAreAdditive() {
        val approval = ServerEvent.ApprovalRequested("s", "live", "Bash: rg foo notes.md", "/work", "rg")
        assertEquals(approval, decodeEvent(
            """{"type":"approval_requested","session":"s","request_id":"live","summary":"Bash: rg foo notes.md","shell_directory":"/work","shell_prefix":"rg"}""",
        ))
        val rule = ShellApprovalRule("rule", "/work", "rg foo notes.md", "rg")
        assertEquals(ServerEvent.WorkerApprovalPolicyChanged(WorkerApprovalPolicy.Ask, listOf(rule)),
            decodeEvent("""{"type":"worker_approval_policy_changed","policy":"ask","shell_rules":[{"id":"rule","directory":"/work","command":"rg foo notes.md","prefix":"rg"}]}"""))
        assertMessage(
            """{"type":"remember_shell_approval","request_id":"live","prefix":"rg"}""",
            ClientMessage.RememberShellApproval("live", "rg"),
        )
    }

    @Test fun chatWaitingOverridesAndDrainStatus() {
        val event = decodeEvent(
            """{"type":"chat_waiting","waiting":{"workspace":{"kind":"self_update","stage":"wait"},"threads":{"active":null,"queued":{"kind":"queued_turn"}}}}""",
        ) as ServerEvent.WaitingChanged
        assertEquals(WaitReason("self_update"), event.waiting.reason("inherited"))
        assertNull(event.waiting.reason("active"))
        assertEquals(WaitReason("queued_turn"), event.waiting.reason("queued"))

        val drain = decodeEvent(
            """{
                "type":"drain_status",
                "status":{
                    "started_at":1700000000000,"reason":"update","progress":"draining","jobs_running":2,
                    "blocking":{
                        "threads":[],"gate_held":true,"checking_sessions":false,
                        "blockers":[{"workspace":"lab","session":"s","id":"q","title":null,"kind":"question","started_at":1700000000000}]
                    },
                    "can_cancel":true,"message":null
                }
            }""",
        ) as ServerEvent.DrainChanged
        assertEquals(DrainStatus(1700000000000, "update", "draining"), drain.status)
        assertTrue(drain.status.holdingInput)
        assertFalse(drain.status.copy(progress = "cancelled").holdingInput)
    }

    @Test fun workspaceAndThreadSnapshots() {
        assertEquals(ServerEvent.Workspaces(listOf(Workspace("lab", "研究室", "~/lab"))), decodeEvent(
            """{"type":"workspaces","workspaces":[{"id":"lab","path":"~/lab","title":"研究室","last_opened":null,"running":true}]}""",
        ))
        val frame = Protocol.decode(
            """{"seq":72,"workspace":"lab","event":{"type":"threads","threads":[{"id":"first","title":"最初","updated":1700000000000,"pinned":true},{"id":"second","title":"次","updated":1700000000001,"attached":[]}]}}""",
        )
        assertEquals("lab", frame.workspace)
        val threads = (frame.event as ServerEvent.Threads).threads
        assertTrue(threads[0].pinned)
        assertFalse(threads[1].pinned)
        assertFalse(threads[1].archived)
    }

    @Test fun planApprovalAndUnknownEvent() {
        val plan = decodeEvent(
            """{"type":"plan_changed","session":"s","plan":{"goal":"原稿","done_condition":"校正済み","status":"active","pace":"normal","done":[],"next":[{"task":"定義を確認する","agent":null,"reason":"表記の統一"}],"rejected":[],"question":null},"progress":{"state":"paused","resume_at":1700000000}}""",
        ) as ServerEvent.PlanChanged
        assertEquals("定義を確認する", plan.plan.next.single().task)
        assertEquals("paused", plan.progress?.state)
        val approval = decodeEvent(
            """{"type":"approval_requested","session":"s","request_id":"codex:3","actor":{"kind":"codex","thread_id":"c","parent_tool_use":"delegate:2"},"summary":"実行: git diff\n対象: 原稿"}""",
        ) as ServerEvent.ApprovalRequested
        assertEquals("実行: git diff\n対象: 原稿", approval.summary)
        assertEquals(ServerEvent.ApprovalResolved("codex:3"), decodeEvent(
            """{"type":"approval_resolved","request_id":"codex:3","decision":"accept","automatic":true}""",
        ))
        assertEquals(ServerEvent.Unknown("symbol_results"), decodeEvent(
            """{"type":"symbol_results","request":1,"results":{}}""",
        ))
    }

    @Test fun gitHubConnectEventsAreAdditive() {
        assertEquals(ServerEvent.Unknown("git_hub_connect_card"), decodeEvent(
            """{"type":"git_hub_connect_card","session":"s","attempt":"a","state":{"state":"connected","login":"reporter"}}""",
        ))
        assertEquals(ServerEvent.Unknown("git_hub_account_changed"), decodeEvent(
            """{"type":"git_hub_account_changed","state":{"state":"disconnected"}}""",
        ))
    }

    @Test fun protocol65IgnoresAdditiveFieldsAndEvents() {
        assertEquals(ServerEvent.Welcome(65), Protocol.decode(
            """{"seq":0,"future_envelope_field":true,"event":{"type":"welcome","protocol":65,"server":"secretaire","future_event_field":true}}""",
        ).event)
        assertEquals(ServerEvent.Unknown("cards"), decodeEvent(
            """{"type":"cards","deck":"paper","cards":[{"id":"translated","translation":{"status":"stale"}}],"diagnostics":[]}""",
        ))
    }

    @Test fun reviewFindingsEventIsIgnored() {
        assertEquals(ServerEvent.Unknown("review_findings"), decodeEvent(
            """{"type":"review_findings","deck":"paper","outcome":{"kind":"loaded","snapshot":{"open_count":0,"card_open_counts":{},"findings":[]}}}""",
        ))
    }

    @Test fun protocol62WelcomeFromOlderServerIsSupported() {
        val welcome = decodeEvent("""{"type":"welcome","protocol":62}""") as ServerEvent.Welcome
        assertEquals(62, welcome.protocol)
        assertTrue(welcome.protocol in MIN_PROTOCOL_VERSION..PROTOCOL_VERSION)
    }

    @Test fun urlNormalization() {
        assertEquals("http://192.0.2.21:7878/ws", socketUrl("ws://192.0.2.21:7878"))
        assertEquals("https://[2001:db8::21]:7878/ws", socketUrl("https://[2001:db8::21]:7878/ws"))
        assertEquals("https://labo.example/ws", socketUrl("wss://labo.example/ws"))
        assertNull(socketUrl("ws://server/ws?token=secret"))
        assertNull(socketUrl("ws://user:secret@server/ws"))
        assertNull(socketUrl("labo.example"))
    }

    private fun assertMessage(expected: String, message: ClientMessage) =
        assertEquals(Json.parseToJsonElement(expected), Json.parseToJsonElement(Protocol.encode(message)))
}

internal fun decodeEvent(event: String) = Protocol.decode("""{"seq":42,"workspace":"lab","event":$event}""").event
