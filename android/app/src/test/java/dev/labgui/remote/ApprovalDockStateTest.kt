package dev.labgui.remote

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.*
import org.junit.Test

class ApprovalDockStateTest {
    @Test fun snapshotsReplacePreviousItemsAndFilterByWorkspace() {
        val original = queuedApprovalFixture("original")
        val kept = queuedApprovalFixture("kept")
        val snapshot = ApprovalDockState().replace("lab", listOf(original)).replace(
            "lab", listOf(kept, original.copy(workspace = "other"), original.copy(state = ApprovalQueueState.Denied)),
        )
        assertEquals(listOf(kept), snapshot.forThread("s"))
    }

    @Test fun pendingRowsPreserveServerOrderAndExpiryUpdatesTheBadge() {
        val first = queuedApprovalFixture("z")
        val second = queuedApprovalFixture("a")
        val differentThread = queuedApprovalFixture("elsewhere", thread = "t")
        val dock = ApprovalDockState().replace("lab", listOf(first, differentThread, second))
        assertEquals(listOf(first, second), dock.forThread("s"))
        val expired = dock.update(first.copy(state = ApprovalQueueState.Expired))
        assertEquals(listOf(second), expired.forThread("s"))
        assertEquals(mapOf("s" to 1, "t" to 1), expired.countsByThread())
    }

    @Test fun olderPendingUpdatesCannotReplaceANewerItem() {
        val approval = queuedApprovalFixture("a")
        val updated = approval.copy(updatedAt = approval.updatedAt + 1, reason = "Updated")
        val dock = ApprovalDockState().update(updated).update(approval)
        assertEquals(listOf(updated), dock.forThread("s"))
    }

    @Test fun allTerminalStatesRemoveTheMatchingItem() {
        val request = queuedApprovalFixture("a")
        val other = queuedApprovalFixture("b")
        val dock = ApprovalDockState().update(request).update(other)
        for (state in ApprovalQueueState.entries - ApprovalQueueState.Pending) {
            val resolved = dock.update(request.copy(state = state)).update(request.copy(state = state))
            assertEquals(mapOf(other.id to other), resolved.pending)
        }
    }

    @Test fun bulkChoicesMatchKindAndExactScopeAndExcludeIndividualOnlyRequests() {
        val a = queuedApprovalFixture("a")
        val b = queuedApprovalFixture("b", job = "another-job", risk = ApprovalRisk.Mid)
        assertTrue(a.canBatchWith(b))
        assertFalse(a.canBatchWith(b.copy(risk = ApprovalRisk.High)))
        assertFalse(a.canBatchWith(b.copy(unattended = "never-unattended")))
        assertFalse(a.canBatchWith(b.copy(workspace = "other")))
        assertFalse(a.canBatchWith(b.copy(thread = "other")))
        assertFalse(a.canBatchWith(b.copy(action = b.action.copy(tool = "git_commit_scoped"))))
        assertFalse(a.canBatchWith(b.copy(action = b.action.copy(scope = buildJsonObject { put("directory", "/elsewhere") }))))
        assertFalse(a.copy(action = a.action.copy(scope = JsonNull)).let { it.canBatchWith(it) })
    }

    @Test fun integrateCardsListFilesAndFlaggedLinesAsPlainText() {
        val action = queuedApprovalFixture("integrate").action.copy(
            tool = "git_integrate_patch",
            args = Json.parseToJsonElement(
                """{"paths":["docs/a.md","docs/b.md"],"privacy_findings":3,"locations":["docs/b.md:14"],
                    "patch_sha256":"0123456789abcdef","head":"fedcba9876543210"}""",
            ),
            scope = buildJsonObject { put("workspace", "handbook") },
        )
        val card = action.integrateCard()!!
        assertEquals(listOf("docs/a.md", "docs/b.md"), card.files)
        assertEquals(listOf("docs/b.md:14"), card.flagged)
        assertEquals(2, card.unlisted)
        assertEquals(listOf("handbook", "0123456789ab", "fedcba987654"), listOf(card.workspace, card.patch, card.head))
        assertNull(card.branch)
        assertNull(card.detachedHead)
        fun checkout(field: String, value: String) = action.copy(
            args = Json.parseToJsonElement("""{"paths":["docs/a.md"],"$field":"$value"}"""),
        ).integrateCard()!!
        assertEquals("topic/notes", checkout("branch", "topic/notes").branch)
        assertEquals("fedcba987654", checkout("detached_head", "fedcba9876543210").detachedHead)
        // Other tools and redacted records keep the generic card.
        assertNull(action.copy(tool = "fs_write").integrateCard())
        assertNull(action.copy(args = JsonPrimitive("[redacted]")).integrateCard())
    }

    @Test fun requestsWithoutADeadlineRoundTripTheServerSentinel() {
        val waiting = queuedApprovalFixture("integrate", risk = ApprovalRisk.High, expiresAt = APPROVAL_NO_EXPIRY)
            .let { it.copy(unattended = "never-unattended", action = it.action.copy(tool = "git_integrate_patch")) }
        val json = kotlinx.serialization.json.Json.encodeToString(QueuedApproval.serializer(), waiting)
        // fuzukue_proto::APPROVAL_NO_EXPIRY is i64::MAX.
        assertTrue(json.contains("\"expires_at\":9223372036854775807"))
        val decoded = kotlinx.serialization.json.Json.decodeFromString(QueuedApproval.serializer(), json)
        assertNull(decoded.deadline)
        assertEquals(3_800_000_000_000L, queuedApprovalFixture("dated").deadline)
        // A request without a deadline stays pending in the dock until it is decided.
        val dock = ApprovalDockState().replace("lab", listOf(decoded))
        assertEquals(listOf(decoded), dock.forThread("s"))
        assertFalse(decoded.canBatchWith(decoded))
    }

    @Test fun decisionRepliesAreCorrelatedAndInterruptionLeavesTheOutcomeUnknown() {
        val a = queuedApprovalFixture("a")
        val b = queuedApprovalFixture("b")
        val base = ApprovalDecisionProgress(mapOf(1uL to a, 2uL to b))
        val partial = base.complete(2uL, "already decided").complete(2uL, null)
        assertEquals(listOf(ApprovalDecisionFailure(b, "already decided")), partial.failures)
        assertTrue(partial.isSending)
        assertEquals(1, partial.complete(1uL, null).succeeded)
        val interrupted = partial.interrupt("unknown")
        assertFalse(interrupted.isSending)
        assertEquals(listOf("already decided", "unknown"), interrupted.failures.map { it.message })
    }
}

internal fun queuedApprovalFixture(
    id: String, thread: String = "s", job: String = "reviewer", risk: ApprovalRisk = ApprovalRisk.Low,
    createdAt: Long = 1_789_000_000_000L, expiresAt: Long = 3_800_000_000_000L,
): QueuedApproval = QueuedApproval(
    id = id,
    workspace = "lab",
    thread = thread,
    job = job,
    requester = "reviewer",
    risk = risk,
    unattended = "defer-and-continue",
    reason = "対象ファイルの変更を確認するため",
    createdAt = createdAt,
    expiresAt = expiresAt,
    updatedAt = createdAt,
    state = ApprovalQueueState.Pending,
    action = QueuedApprovalAction(
        tool = "fs_write",
        args = buildJsonObject {
            put("path", "notes/$id.md")
            put("diff", "+確認した内容")
        },
        scope = buildJsonObject { put("directory", "/worktree/notes") },
    ),
)
