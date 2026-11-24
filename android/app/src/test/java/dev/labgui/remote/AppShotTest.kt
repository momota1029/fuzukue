package dev.labgui.remote

import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertTextContains
import androidx.compose.ui.test.hasAnyAncestor
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.isDialog
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performScrollToNode
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTextInput
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import org.robolectric.annotation.LooperMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@LooperMode(LooperMode.Mode.PAUSED)
@Config(qualifiers = "ja-rJP-w412dp-h870dp-mdpi")
class AppShotTest {
    @get:Rule val compose = createComposeRule()

    @Test fun settingsLight() = capture("settings-light", "サーバー URL", state = RemoteState())
    @Test fun settingsDark() = capture("settings-dark", "サーバー URL", dark = true, state = RemoteState())
    @Test
    @Config(qualifiers = "en-rUS-w412dp-h870dp-mdpi")
    fun settingsEnglish() = capture("settings-en", "Server URL", state = RemoteState())
    @Test fun threadsLight() = capture("threads-light", "ピン留め", state = connected)
    @Test fun threadsDark() = capture("threads-dark", "ピン留め", dark = true, state = connected)
    @Test
    @Config(qualifiers = "en-rUS-w412dp-h870dp-mdpi")
    fun threadsEnglish() = capture("threads-en", "Pinned", state = connected)
    @Test fun runningLight() = capture("running-light", progress, state = running)
    @Test fun runningDark() = capture("running-dark", progress, dark = true, state = running)
    @Test fun approvalLight() = capture("approval-light", approval.summary, state = withApproval)
    @Test fun approvalDark() = capture("approval-dark", approval.summary, dark = true, state = withApproval)

    @Test fun approvalDockOpensAboveTheComposerAndReviewsAnExplicitBulkSelection() {
        val a = queuedApprovalFixture("a", risk = ApprovalRisk.Mid)
        val b = queuedApprovalFixture("b")
        val high = queuedApprovalFixture("sensitive", risk = ApprovalRisk.High, job = "another-job")
        val state = mutableStateOf(running.copy(
            approvalDock = ApprovalDockState().replace("lab", listOf(a, b, high)),
        ))
        val submitted = mutableListOf<Pair<List<String>, Boolean>>()
        compose.setContent {
            RemoteTheme {
                RemoteApp(state.value, onDecideQueuedApprovals = { target, approvals, allow ->
                    assertEquals(ConversationTarget("lab", "s"), target)
                    submitted.add(approvals.map { it.id } to allow)
                })
            }
        }
        val toggle = compose.onNodeWithTag("approval-dock-toggle")
        val list = compose.onNodeWithTag("approval-dock-list")
        toggle.assertIsDisplayed()
        val composer = compose.onNodeWithContentDescription("メッセージ")
        composer.assertIsDisplayed()
        assertTrue(toggle.fetchSemanticsNode().boundsInRoot.bottom <= composer.fetchSemanticsNode().boundsInRoot.top)
        list.assertDoesNotExist()
        compose.onRoot().captureRoboImage("build/shots/approval-dock-collapsed.png")

        toggle.performClick()
        list.assertIsDisplayed()
        compose.onNodeWithText("中リスク").assertIsDisplayed()
        composer.assertIsDisplayed()
        assertTrue(list.fetchSemanticsNode().boundsInRoot.bottom <= composer.fetchSemanticsNode().boundsInRoot.top)
        compose.onRoot().captureRoboImage("build/shots/approval-dock-expanded.png")

        val highRisk = hasText("高リスク")
        list.performScrollToNode(highRisk)
        compose.onNode(highRisk).assertIsDisplayed()
        compose.onNode(hasAnyAncestor(hasTestTag("approval-row-sensitive")) and hasText("一括操作に選択"))
            .assertDoesNotExist()

        for (id in listOf("a", "b")) {
            val checkbox = hasContentDescription("fs_write · notes/$id.md を 一括操作に選択")
            list.performScrollToNode(checkbox)
            compose.onNode(checkbox).performClick()
        }
        compose.onNodeWithText("一括許可（2 件）")
            .assertIsDisplayed()
            .performSemanticsAction(SemanticsActions.OnClick)
        val inDialog = hasAnyAncestor(isDialog())
        compose.onNode(inDialog and hasText("一括操作の対象を確認"), useUnmergedTree = true).assertIsDisplayed()
        for (id in listOf("a", "b")) {
            val target = RuntimeEnvironment.getApplication().getString(R.string.approval_dock_target, "notes/$id.md")
            compose.onNode(inDialog and hasText(target), useUnmergedTree = true)
                .performScrollTo().assertIsDisplayed()
        }
        compose.runOnIdle { assertEquals(emptyList<Pair<List<String>, Boolean>>(), submitted) }
        compose.onNode(inDialog and hasText("一括許可（2 件）"), useUnmergedTree = true).performClick()
        compose.runOnIdle {
            assertEquals(listOf(listOf("a", "b") to true), submitted)
            state.value = state.value.copy(approvalDock = ApprovalDockState())
        }
        toggle.assertDoesNotExist()
        compose.onNodeWithContentDescription("メッセージ").assertIsDisplayed()
    }

    @Test fun approvalNoticeExpandsTheTargetThreadsDockWhenTheSnapshotArrives() {
        val approval = queuedApprovalFixture("from-notice")
        val state = mutableStateOf(running)
        compose.setContent {
            RemoteTheme {
                RemoteApp(state.value, openApprovalDock = ConversationTarget("lab", "s") to 1)
            }
        }
        compose.onNodeWithTag("approval-dock-toggle").assertDoesNotExist()
        compose.runOnIdle { state.value = state.value.copy(approvalDock = ApprovalDockState().update(approval)) }
        compose.onNodeWithTag("approval-dock-list").assertIsDisplayed()
    }

    @Test fun savingAnApprovalRequiresShowingTheExactCommandAndDirectory() {
        val request = ServerEvent.ApprovalRequested("s", "shell", "Bash: cat notes.md", "/worktree/notes")
        val saved = mutableListOf<Pair<String, String?>>()
        compose.setContent {
            RemoteTheme {
                RemoteApp(
                    withApproval.copy(approvals = mapOf(request.requestId to request), shellApprovalRules = emptyList()),
                    onRememberShellApproval = { id, prefix -> saved.add(id to prefix) },
                )
            }
        }
        val remember = compose.onNodeWithText("このコマンドを常に許可")
        remember.assertIsDisplayed()
        compose.onNodeWithText("cat … を常に許可").assertDoesNotExist()
        compose.onRoot().captureRoboImage("build/shots/approval-remember.png")
        remember.performClick()
        compose.onNodeWithText("/worktree/notes").assertIsDisplayed()
        compose.runOnIdle { assertEquals(emptyList<Pair<String, String?>>(), saved) }
        compose.onNodeWithText("保存して許可").performClick()
        compose.runOnIdle { assertEquals(listOf("shell" to null), saved) }
    }

    @Test fun eligibleApprovalOffersExactAndLiteralPrefixChoices() {
        val request = ServerEvent.ApprovalRequested("s", "shell", "Bash: rg word notes.md", "/worktree/notes", "rg")
        val saved = mutableListOf<Pair<String, String?>>()
        compose.setContent {
            RemoteTheme {
                RemoteApp(
                    withApproval.copy(approvals = mapOf(request.requestId to request), shellApprovalRules = emptyList()),
                    onRememberShellApproval = { id, prefix -> saved.add(id to prefix) },
                )
            }
        }
        val prefix = compose.onNodeWithText("rg … を常に許可")
        prefix.assertIsDisplayed()
        compose.onRoot().captureRoboImage("build/shots/approval-prefix.png")
        compose.onNodeWithText("このコマンドを常に許可").performClick()
        compose.onNodeWithText("このコマンドだけ").assertIsDisplayed()
        compose.onNodeWithText("キャンセル").performClick()
        prefix.performClick()
        compose.onNodeWithText("先頭が同じコマンド（rg …）").assertIsDisplayed()
        compose.onNodeWithText("/worktree/notes").assertIsDisplayed()
        compose.runOnIdle { assertEquals(emptyList<Pair<String, String?>>(), saved) }
        compose.onNodeWithText("保存して許可").performClick()
        compose.runOnIdle { assertEquals(listOf("shell" to "rg"), saved) }
    }

    @Test fun workspaceApprovalSettingsOfferPolicyAndRuleRevocation() {
        val policies = mutableListOf<WorkerApprovalPolicy>()
        val revoked = mutableListOf<String>()
        compose.setContent {
            RemoteTheme {
                RemoteApp(
                    connected.copy(
                        workerApprovalPolicy = WorkerApprovalPolicy.Ask,
                        shellApprovalRules = listOf(ShellApprovalRule("rule", "/worktree/notes", "cat notes.md", "cat")),
                    ),
                    showSettingsInitially = true,
                    onWorkerApprovalPolicy = { policies.add(it) },
                    onRevokeShellApproval = { revoked.add(it) },
                )
            }
        }
        compose.onNodeWithText("方針役に相談").performScrollTo().performClick()
        val revoke = compose.onNodeWithText("許可を取り消す").performScrollTo()
        compose.onNodeWithText("cat …").assertIsDisplayed()
        compose.onRoot().captureRoboImage("build/shots/approval-settings.png")
        revoke.performClick()
        compose.runOnIdle {
            assertEquals(listOf(WorkerApprovalPolicy.Advisor), policies)
            assertEquals(listOf("rule"), revoked)
        }
    }

    @Test fun protocolMismatchOffersTheSameUpdateCheckAndDownload() {
        val url = "https://example.org/android-latest.json"
        val updates = mutableStateOf(UpdateState(url))
        var downloads = 0
        compose.setContent {
            RemoteTheme {
                RemoteApp(
                    RemoteState(error = "プロトコルのバージョンが一致しません", updateRequired = true),
                    updates = updates.value,
                    onCheckUpdate = {
                        assertEquals(url, it)
                        updates.value = UpdateState(url, UpdateStatus.Available(UpdateRelease(
                            version = "0.2.0",
                            versionCode = 3,
                            packageName = BuildConfig.APPLICATION_ID,
                            sha256 = "a".repeat(64),
                            apk = "fuzukue-android-0.2.0.apk",
                            minimumProtocol = 18,
                            releaseNotesFirstLine = "接続を改善",
                            builtAt = "2026-11-22T08:00:00Z",
                        )))
                    },
                    onDownloadUpdate = { downloads++ },
                )
            }
        }
        compose.onNodeWithText("アプリを更新してください").assertIsDisplayed()
        compose.onNodeWithText("更新を確認").performScrollTo().performClick()
        compose.onNodeWithText("新しい版 v0.2.0 があります（接続を改善）").performScrollTo().assertIsDisplayed()
        compose.runOnIdle { assertEquals(0, downloads) }
        val download = compose.onNodeWithText("APK をダウンロードしてインストール").performScrollTo()
        compose.onRoot().captureRoboImage("build/shots/update-available.png")
        download.performClick()
        compose.runOnIdle { assertEquals(1, downloads) }
    }

    @Test fun draftSurvivesReconnectAndStopMovesAboveThePillWhileSending() {
        val state = mutableStateOf(running)
        val sent = mutableListOf<Pair<String, String>>()
        val stopped = mutableListOf<String>()
        val attachments = Attachments(
            RuntimeEnvironment.getApplication(), canUpload = { false }, awaitUpload = { false }, send = { false },
        )
        compose.setContent {
            RemoteTheme {
                RemoteApp(
                    state.value,
                    onSend = { session, text -> sent.add(session to text) },
                    onStopTurn = { stopped.add(it) },
                    attachments = attachments,
                )
            }
        }

        compose.onNodeWithText("作業中…", substring = true).performClick()
        val inActivityDetails = hasAnyAncestor(hasTestTag("activity-details"))
        compose.onNode(hasText("数式チェック") and inActivityDetails).assertIsDisplayed()
        compose.onNode(hasText("閉じる") and inActivityDetails).performClick()
        val draft = compose.onNode(hasSetTextAction())
        val send = compose.onNodeWithTag("composer-send")
        val stop = compose.onNodeWithTag("composer-stop")
        send.assertDoesNotExist()
        stop.assertIsEnabled()
        compose.onNodeWithTag("composer-attach").assertIsEnabled()
        draft.performTextInput("下書き")

        compose.runOnIdle { state.value = running.copy(phase = ConnectionPhase.Retrying) }
        send.assertIsNotEnabled()
        draft.assertTextContains("下書き")

        compose.runOnIdle { state.value = running }
        stop.assertIsEnabled()
        compose.onRoot().captureRoboImage("build/shots/composer-running.png")
        stop.performClick()
        draft.assertTextContains("下書き")
        send.assertIsEnabled().performClick()
        compose.runOnIdle {
            assertEquals(listOf("s"), stopped)
            assertEquals(listOf("s" to "下書き"), sent)
        }
        send.assertDoesNotExist()
        stop.assertIsEnabled()
    }

    @Test fun routinePlanIsCollapsedAndInlineSummaryIsOptIn() {
        val showPlan = mutableStateOf(false)
        compose.setContent {
            RemoteTheme {
                RemoteApp(running, showPlanAboveConversation = showPlan.value,
                    onShowPlanAboveConversation = { showPlan.value = it })
            }
        }
        compose.onNodeWithTag("plan-toggle").assertIsDisplayed()
        compose.onNodeWithTag("plan-summary").assertDoesNotExist()
        compose.onNodeWithTag("plan-attention", useUnmergedTree = true).assertDoesNotExist()
        compose.onNodeWithTag("plan-sheet").assertDoesNotExist()
        compose.onNodeWithText("独立性が必要な式を確認する").assertDoesNotExist()
        compose.onNode(hasSetTextAction()).assertIsDisplayed()
        compose.onRoot().captureRoboImage("build/shots/plan-collapsed.png")

        compose.onNodeWithText("設定").performClick()
        compose.onNodeWithContentDescription("プランを会話の上に表示").performScrollTo().performClick()
        compose.onNodeWithText("会話に戻る").performClick()
        compose.onNodeWithTag("plan-summary").assertIsDisplayed()
        compose.onNodeWithTag("plan-attention", useUnmergedTree = true).assertDoesNotExist()
    }

    @Test
    @Config(qualifiers = "ja-rJP-w640dp-h360dp-land-mdpi")
    fun blockedPlanStaysOpenAcrossRecompositionAndCanBeDismissed() {
        val plan = Plan("waiting_for_user", listOf(PlanStep("資料を照合する", PlanBlocker("版を選ぶ"))),
            question = "どの版を使う？", goal = "参考文献をそろえる")
        val timeline = running.timelines.getValue("s").reduce(ServerEvent.PlanChanged("s", plan))
        val state = mutableStateOf(running.copy(timelines = mapOf("s" to timeline)))
        compose.setContent { RemoteTheme { RemoteApp(state.value) } }

        compose.onNodeWithTag("plan-sheet").assertDoesNotExist()
        compose.onNodeWithTag("plan-attention", useUnmergedTree = true).assertIsDisplayed()
        compose.onNodeWithTag("plan-toggle").performClick()
        compose.onNodeWithTag("plan-sheet").assertIsDisplayed().captureRoboImage("build/shots/plan-sheet.png")
        compose.onNodeWithText("参考文献をそろえる").assertIsDisplayed()
        compose.onNodeWithText("確認待ち").assertIsDisplayed()
        compose.onNodeWithText("どの版を使う？").performScrollTo().assertIsDisplayed()
        compose.onNodeWithText("資料を照合する").performScrollTo().assertIsDisplayed()

        compose.runOnIdle {
            val changedPlan = plan.copy(question = "新版を使う？")
            state.value = state.value.copy(
                timelines = mapOf("s" to timeline.reduce(ServerEvent.PlanChanged("s", changedPlan))),
            )
        }
        compose.onNodeWithText("新版を使う？").performScrollTo().assertIsDisplayed()
        compose.onNodeWithText("閉じる").performScrollTo().performClick()
        compose.onNodeWithTag("plan-sheet").assertDoesNotExist()
        compose.onNode(hasSetTextAction()).assertIsDisplayed()
    }

    @Test
    @Config(qualifiers = "ja-rJP-w1080dp-h760dp-land-mdpi")
    fun tabletLight() = capture("tablet-light", progress, state = running, tablet = true)

    @Test
    @Config(qualifiers = "ja-rJP-w1080dp-h760dp-land-mdpi")
    fun tabletDark() = capture("tablet-dark", progress, dark = true, state = running, tablet = true)

    private fun capture(name: String, expected: String, state: RemoteState, dark: Boolean = false, tablet: Boolean = false) {
        compose.setContent {
            RemoteTheme(dark) {
                RemoteApp(
                    state,
                    initialUrl = "ws://your-tailnet-host.example:7878/ws",
                    initialToken = "fixture-token",
                    background = true,
                )
            }
        }
        compose.onNodeWithText(expected).assertIsDisplayed()
        if (tablet) compose.onNodeWithText("ピン留め").assertIsDisplayed()
        compose.onRoot().captureRoboImage("build/shots/$name.png")
    }

    private val connected = RemoteState(
        phase = ConnectionPhase.Connected,
        workspace = "lab",
        workspaces = listOf(Workspace("lab", "確率論ノート", "~/probability")),
        threads = listOf(
            ThreadInfo("s", "補題の仮定を確認", 100, pinned = true),
            ThreadInfo("t", "参考文献の整理", 200),
            ThreadInfo("u", "章立ての見直し", 150),
        ),
    )
    private val progress = "補題 2 を読み、独立性を使う箇所を確認中。"
    private val running = connected.copy(
        selection = "s",
        timelines = mapOf("s" to Timeline(
            rows = linkedMapOf(
                "user" to TimelineRow.Message("補題 2 の仮定を確認して。", user = true),
                "reply" to TimelineRow.Message("**独立性**が必要な箇所を確認するね。\n\n" +
                    "たとえば ${'$'}X_n${'$'} の期待値を見ていく。\n\n" +
                    "${'$'}${'$'}E[X_n] = 0${'$'}${'$'}", user = false),
                "delegate:one" to TimelineRow.Job(
                    JobInfo("gpt-6.2-sol", "数式チェック", System.currentTimeMillis() - 12_000),
                    summary = progress,
                ),
            ),
            running = true,
            plan = Plan("active", listOf(PlanStep("独立性が必要な式を確認する"))),
        )),
    )
    private val approval = ServerEvent.ApprovalRequested("s", "request-1", "ファイルの確認\ngit diff -- notes/lemma.md")
    private val withApproval = running.copy(approvals = mapOf(approval.requestId to approval))
}
