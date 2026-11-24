package dev.labgui.remote

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlinx.serialization.json.JsonObject
import java.text.DateFormat
import java.util.Date

internal data class ApprovalRiskColors(val container: Color, val content: Color)

internal val LocalApprovalMidRiskColors = staticCompositionLocalOf<ApprovalRiskColors> {
    error("Approval risk colors must be provided by RemoteTheme")
}

private data class BulkApprovalReview(val approvals: List<QueuedApproval>, val accept: Boolean)

@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun ApprovalDock(
    approvals: List<QueuedApproval>,
    progress: ApprovalDecisionProgress?,
    enabled: Boolean,
    maxExpandedHeight: Dp,
    openRequest: Int?,
    jobTitle: (String) -> String,
    onDecide: (List<QueuedApproval>, Boolean) -> Unit,
    onDismissResult: () -> Unit,
    loadError: String?,
    onRetry: () -> Unit,
) {
    var expanded by rememberSaveable { mutableStateOf(false) }
    var handledOpenRequest by rememberSaveable { mutableStateOf<Int?>(null) }
    var selectedIds by remember { mutableStateOf(emptySet<String>()) }
    var review by remember { mutableStateOf<BulkApprovalReview?>(null) }
    var showFailures by remember { mutableStateOf(false) }
    LaunchedEffect(openRequest) {
        if (openRequest != null && openRequest != handledOpenRequest) {
            handledOpenRequest = openRequest
            expanded = true
        }
    }
    val selectableIds = remember(approvals) {
        approvals.filter { it.canBatchWith(it) }.groupBy { it.action.tool to it.action.scope }
            .values.filter { it.size > 1 }.flatten().mapTo(mutableSetOf()) { it.id }
    }
    LaunchedEffect(selectableIds) {
        selectedIds = selectedIds.intersect(selectableIds)
    }

    val showProgress = progress != null &&
        (progress.isSending || progress.failures.isNotEmpty() || progress.succeeded > 1)
    if (approvals.isEmpty() && !showProgress && loadError == null && review == null) return

    val canDecide = enabled && progress?.isSending != true
    val selected = approvals.filter { it.id in selectedIds && it.id in selectableIds }
    val firstSelected = selected.firstOrNull()
    val first = approvals.firstOrNull()
    BackHandler(enabled = expanded && first != null) { expanded = false }

    Surface(Modifier.fillMaxWidth(), color = MaterialTheme.colorScheme.surfaceContainer) {
        Column {
            if (loadError != null) {
                Row(
                    Modifier.fillMaxWidth().padding(horizontal = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(loadError, Modifier.weight(1f).semantics { liveRegion = LiveRegionMode.Polite },
                        color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
                    TextButton(onClick = onRetry, enabled = enabled) {
                        Text(stringResource(R.string.approval_dock_retry))
                    }
                }
            }
            if (showProgress && progress != null) {
                ApprovalProgress(progress, onShowFailures = { showFailures = true }, onDismiss = onDismissResult)
            }
            if (first != null) {
                val toggle = stringResource(if (expanded) R.string.approval_dock_collapse else R.string.approval_dock_expand)
                Row(
                    Modifier.fillMaxWidth()
                        .testTag("approval-dock-toggle")
                        .clickable(role = Role.Button, onClickLabel = toggle) { expanded = !expanded }
                        .heightIn(min = 48.dp)
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(pluralStringResource(R.plurals.approval_count, approvals.size, approvals.size),
                            style = MaterialTheme.typography.labelLarge)
                        Text(
                            "${first.action.tool} · ${first.action.targetText()}",
                            style = MaterialTheme.typography.bodySmall,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    Text(if (expanded) "▾" else "▴", Modifier.clearAndSetSemantics {},
                        color = MaterialTheme.colorScheme.primary)
                }
                if (expanded && maxExpandedHeight > 0.dp) {
                    val groups = remember(approvals) { approvals.groupBy { it.job } }
                    Column(Modifier.heightIn(max = maxExpandedHeight)) {
                        LazyColumn(
                            Modifier.weight(1f, fill = false).fillMaxWidth().testTag("approval-dock-list"),
                            contentPadding = PaddingValues(start = 12.dp, end = 12.dp, bottom = 8.dp),
                            verticalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            for ((job, jobApprovals) in groups) {
                                item(key = "job/$job") {
                                    Text(
                                        stringResource(R.string.approval_dock_job, jobTitle(job)),
                                        Modifier.padding(start = 4.dp).semantics { heading() },
                                        style = MaterialTheme.typography.labelMedium,
                                    )
                                }
                                items(jobApprovals, key = { "approval/${it.id}" }) { approval ->
                                    ApprovalDockRow(
                                        approval,
                                        enabled = canDecide,
                                        selectable = approval.id in selectableIds,
                                        selected = approval.id in selectedIds,
                                        selectionEnabled = canDecide &&
                                            (firstSelected == null || firstSelected.canBatchWith(approval)),
                                        onSelect = { checked ->
                                            selectedIds = if (checked) selectedIds + approval.id else selectedIds - approval.id
                                        },
                                        onDecide = { accept -> onDecide(listOf(approval), accept) },
                                    )
                                }
                            }
                        }
                        if (selected.size >= 2) {
                            HorizontalDivider()
                            FlowRow(
                                Modifier.fillMaxWidth().padding(horizontal = 12.dp),
                                horizontalArrangement = Arrangement.End,
                            ) {
                                TextButton(onClick = { review = BulkApprovalReview(selected, false) }, enabled = canDecide) {
                                    Text(stringResource(R.string.approval_bulk_deny, selected.size))
                                }
                                TextButton(onClick = { review = BulkApprovalReview(selected, true) }, enabled = canDecide) {
                                    Text(stringResource(R.string.approval_bulk_allow, selected.size))
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    review?.let { batch ->
        BulkApprovalDialog(batch, canDecide, onDismiss = { review = null }) {
            review = null
            selectedIds = emptySet()
            onDecide(batch.approvals, batch.accept)
        }
    }
    if (showFailures && progress != null && progress.failures.isNotEmpty()) {
        AlertDialog(
            onDismissRequest = { showFailures = false },
            title = { Text(stringResource(R.string.approval_dock_results)) },
            text = {
                Column(Modifier.heightIn(max = 320.dp).verticalScroll(rememberScrollState()),
                    verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    for (failure in progress.failures) {
                        Text("${failure.approval.action.tool} · ${failure.approval.action.targetText()}")
                        Text(failure.message, style = MaterialTheme.typography.bodySmall)
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = { showFailures = false }) { Text(stringResource(R.string.close)) }
            },
        )
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ApprovalDockRow(
    approval: QueuedApproval,
    enabled: Boolean,
    selectable: Boolean,
    selected: Boolean,
    selectionEnabled: Boolean,
    onSelect: (Boolean) -> Unit,
    onDecide: (Boolean) -> Unit,
) {
    var details by rememberSaveable { mutableStateOf(false) }
    val target = approval.action.targetText()
    val context = "${approval.action.tool} · $target"
    val expires = remember(approval.expiresAt) {
        DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.MEDIUM).format(Date(approval.expiresAt))
    }
    Surface(color = MaterialTheme.colorScheme.surface, shape = MaterialTheme.shapes.medium) {
        Column(Modifier.fillMaxWidth().testTag("approval-row-${approval.id}").padding(horizontal = 12.dp, vertical = 8.dp)) {
            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text(approval.action.tool, Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                ApprovalRiskLabel(approval.risk)
            }
            ApprovalField(R.string.approval_dock_target, target, lines = 2)
            ApprovalField(R.string.approval_dock_scope, approval.action.scope.displayText(), lines = 2)
            ApprovalField(R.string.approval_dock_reason, approval.reason, lines = if (details) Int.MAX_VALUE else 3)
            Text(stringResource(R.string.approval_dock_expires, expires), style = MaterialTheme.typography.bodySmall)
            if (details) {
                SelectionContainer {
                    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        ApprovalField(R.string.approval_dock_target, target)
                        ApprovalField(R.string.approval_dock_scope, approval.action.scope.displayText())
                        val args = approval.action.args
                        val diff = (args as? JsonObject)?.get("diff") ?: (approval.action.scope as? JsonObject)?.get("diff")
                        val arguments = if (args is JsonObject && diff != null) JsonObject(args - "diff") else args
                        ApprovalField(R.string.approval_dock_arguments, arguments.displayText())
                        if (diff != null) ApprovalField(R.string.approval_dock_diff, diff.displayText())
                        ApprovalField(R.string.approval_dock_requester, approval.requester)
                        Text(stringResource(R.string.approval_dock_once_scope), style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
            if (selectable) {
                val select = stringResource(R.string.approval_dock_select)
                val selectLabel = stringResource(R.string.approval_dock_action_label, select, context)
                Row(
                    Modifier.fillMaxWidth().heightIn(min = 48.dp)
                        .toggleable(selected, enabled = selectionEnabled, role = Role.Checkbox, onValueChange = onSelect)
                        .semantics { contentDescription = selectLabel },
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Checkbox(checked = selected, onCheckedChange = null, enabled = selectionEnabled)
                    Text(select, style = MaterialTheme.typography.labelLarge)
                }
            }
            FlowRow(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                val detailsLabel = stringResource(if (details) R.string.approval_dock_hide_details else R.string.approval_dock_details)
                TextButton(onClick = { details = !details }, modifier = approvalActionLabel(detailsLabel, context)) {
                    Text(detailsLabel)
                }
                val deny = stringResource(R.string.deny)
                TextButton(onClick = { onDecide(false) }, enabled = enabled, modifier = approvalActionLabel(deny, context)) {
                    Text(deny)
                }
                val allow = stringResource(R.string.approval_dock_allow_once)
                Button(onClick = { onDecide(true) }, enabled = enabled, modifier = approvalActionLabel(allow, context)) {
                    Text(allow)
                }
            }
        }
    }
}

@Composable
private fun approvalActionLabel(label: String, target: String): Modifier {
    val description = stringResource(R.string.approval_dock_action_label, label, target)
    return Modifier.heightIn(min = 48.dp).semantics { contentDescription = description }
}

@Composable
private fun QueuedApprovalAction.targetText(): String =
    targetLabel() ?: stringResource(R.string.approval_dock_no_target)

@Composable
private fun ApprovalField(label: Int, value: String, lines: Int = Int.MAX_VALUE) {
    Text(
        stringResource(label, value),
        style = MaterialTheme.typography.bodySmall,
        maxLines = lines,
        overflow = TextOverflow.Ellipsis,
    )
}

@Composable
private fun ApprovalRiskLabel(risk: ApprovalRisk) {
    val colors = MaterialTheme.colorScheme
    val (label, palette) = when (risk) {
        ApprovalRisk.Low -> R.string.approval_risk_low to ApprovalRiskColors(colors.surfaceContainerHigh, colors.onSurface)
        ApprovalRisk.Mid -> R.string.approval_risk_mid to LocalApprovalMidRiskColors.current
        ApprovalRisk.High -> R.string.approval_risk_high to ApprovalRiskColors(colors.errorContainer, colors.onErrorContainer)
    }
    Surface(color = palette.container, contentColor = palette.content, shape = MaterialTheme.shapes.small) {
        Text(stringResource(label), Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            style = MaterialTheme.typography.labelMedium)
    }
}

@Composable
private fun BulkApprovalDialog(
    batch: BulkApprovalReview, enabled: Boolean, onDismiss: () -> Unit, onConfirm: () -> Unit,
) {
    val first = batch.approvals.first()
    val label = stringResource(if (batch.accept) R.string.approval_bulk_allow else R.string.approval_bulk_deny, batch.approvals.size)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.approval_bulk_review)) },
        text = {
            Column(Modifier.heightIn(max = 320.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(first.action.tool)
                ApprovalField(R.string.approval_dock_scope, first.action.scope.displayText())
                if (batch.accept) Text(stringResource(R.string.approval_dock_once_scope))
                for (approval in batch.approvals) {
                    ApprovalField(R.string.approval_dock_target, approval.action.targetText())
                }
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
        confirmButton = { TextButton(onClick = onConfirm, enabled = enabled) { Text(label) } },
    )
}

@Composable
private fun ApprovalProgress(progress: ApprovalDecisionProgress, onShowFailures: () -> Unit, onDismiss: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            if (progress.isSending) stringResource(R.string.approval_dock_sending)
            else stringResource(R.string.approval_dock_result_summary, progress.succeeded, progress.failures.size),
            Modifier.weight(1f).semantics { liveRegion = LiveRegionMode.Polite },
            style = MaterialTheme.typography.labelMedium,
        )
        if (!progress.isSending && progress.failures.isNotEmpty()) {
            TextButton(onClick = onShowFailures) { Text(stringResource(R.string.approval_dock_results)) }
        }
        if (!progress.isSending) TextButton(onClick = onDismiss) { Text(stringResource(R.string.close)) }
    }
}

@Composable
internal fun ApprovalOutcomeRow(row: TimelineRow.ApprovalOutcome) {
    val label = when (row.approval.state) {
        ApprovalQueueState.Granted -> R.string.approval_outcome_granted
        ApprovalQueueState.Denied -> R.string.approval_outcome_denied
        ApprovalQueueState.Expired -> R.string.approval_outcome_expired
        ApprovalQueueState.Withdrawn -> R.string.approval_outcome_withdrawn
        ApprovalQueueState.Pending -> return
    }
    val outcome = stringResource(label, row.approval.action.tool, row.approval.action.targetText())
    Text(
        if (row.decidedHere) stringResource(R.string.approval_outcome_local, outcome) else outcome,
        Modifier.padding(horizontal = 8.dp),
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.bodySmall,
    )
}
