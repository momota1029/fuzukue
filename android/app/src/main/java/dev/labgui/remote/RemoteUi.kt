package dev.labgui.remote

import androidx.activity.compose.BackHandler
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.NestedScrollSource
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Velocity
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlin.time.Duration.Companion.nanoseconds

@Composable
internal fun RemoteTheme(dark: Boolean = isSystemInDarkTheme(), content: @Composable () -> Unit) {
    val colors = if (dark) darkColorScheme(
        primary = Color(0xFFC4C2FF), onPrimary = Color(0xFF292458),
        primaryContainer = Color(0xFF383464), onPrimaryContainer = Color(0xFFE9E6FF),
        background = Color(0xFF171823), surface = Color(0xFF1D1F2C),
        surfaceContainer = Color(0xFF252737), surfaceContainerHigh = Color(0xFF303247),
        outlineVariant = Color(0xFF42455A),
    ) else lightColorScheme(
        primary = Color(0xFF504894), onPrimary = Color.White,
        primaryContainer = Color(0xFFEAE6FF), onPrimaryContainer = Color(0xFF30286B),
        background = Color(0xFFF8F8FD), surface = Color(0xFFFFFBFF),
        surfaceContainer = Color(0xFFF0EFF9), surfaceContainerHigh = Color(0xFFE7E6F4),
        outlineVariant = Color(0xFFD6D5E4),
    )
    val mediumRisk = if (dark) ApprovalRiskColors(Color(0xFF564014), Color(0xFFFFDF9B))
        else ApprovalRiskColors(Color(0xFFFFE5AB), Color(0xFF432D00))
    CompositionLocalProvider(LocalApprovalMidRiskColors provides mediumRisk) {
        MaterialTheme(colorScheme = colors, content = content)
    }
}

@Composable
internal fun RemoteApp(
    state: RemoteState,
    initialUrl: String = "",
    initialToken: String = "",
    settingsError: String? = null,
    background: Boolean = false,
    workerNotices: Boolean = false,
    showPlanAboveConversation: Boolean = false,
    openConversation: Int = 0,
    openApprovalDock: Pair<ConversationTarget, Int>? = null,
    showSettingsInitially: Boolean = state.workspace == null,
    updates: UpdateState = UpdateState(),
    installError: String? = null,
    onCheckUpdate: (String) -> Unit = {},
    onDownloadUpdate: () -> Unit = {},
    onInstallUpdate: () -> Unit = {},
    onAllowUpdate: () -> Unit = {},
    onCancelUpdate: () -> Unit = {},
    onAutoUpdate: (Boolean) -> Unit = {},
    onUpdateChannel: (UpdateChannel) -> Unit = {},
    onUpdateOrigin: (UpdateSourcePreference) -> Unit = {},
    onBackground: (Boolean) -> Unit = {},
    onWorkerNotices: (Boolean) -> Unit = {},
    onShowPlanAboveConversation: (Boolean) -> Unit = {},
    onConnect: (String, String) -> Unit = { _, _ -> },
    onDisconnect: () -> Unit = {},
    onWorkspace: (String) -> Unit = {},
    onThread: (String?) -> Unit = {},
    onNewThread: () -> Unit = {},
    onOlderHistory: (ConversationTarget) -> Unit = {},
    onRequestJob: (String, String, Boolean) -> Unit = { _, _, _ -> },
    onToolPayload: (String, String) -> Unit = { _, _ -> },
    onSend: (String, String) -> Boolean = { _, _ -> false },
    onStopTurn: (String) -> Unit = {},
    onStopWorker: (String, String) -> Unit = { _, _ -> },
    onResolve: (String, Boolean) -> Unit = { _, _ -> },
    onDecideQueuedApprovals: (ConversationTarget, List<QueuedApproval>, Boolean) -> Unit = { _, _, _ -> },
    onDismissQueuedDecision: (ConversationTarget) -> Unit = {},
    onRetryApprovalQueue: (ConversationTarget) -> Unit = {},
    onRememberShellApproval: (String, String?) -> Unit = { _, _ -> },
    onRevokeShellApproval: (String) -> Unit = {},
    onWorkerApprovalPolicy: (WorkerApprovalPolicy) -> Unit = {},
    attachments: Attachments? = null,
    drafts: ConversationDrafts? = null,
) {
    var settings by rememberSaveable { mutableStateOf(showSettingsInitially) }
    var licenses by rememberSaveable { mutableStateOf(false) }
    var connecting by rememberSaveable { mutableStateOf(false) }
    var url by rememberSaveable { mutableStateOf(initialUrl) }
    var token by remember { mutableStateOf(initialToken) }
    var updateUrl by rememberSaveable(updates.channel) { mutableStateOf(updates.url) }
    val conversationState = rememberSaveableStateHolder()
    val selected = state.selection
    val attachmentView = attachments?.view?.collectAsState()?.value ?: AttachmentView()
    val shareTarget = state.workspace?.let { workspace ->
        selected?.takeIf { !state.changingWorkspace && state.threads.any { thread -> thread.id == it } }
            ?.let { ConversationTarget(workspace, it) }
    }
    LaunchedEffect(attachmentView.share, shareTarget) {
        if (attachmentView.share != null && shareTarget != null) attachments?.acceptShare(shareTarget)
    }
    val workspaceTitle = state.workspaces.find { it.id == state.workspace }?.title
    LaunchedEffect(state.phase) {
        if (connecting && state.phase == ConnectionPhase.Connected) {
            settings = false
            connecting = false
        }
    }
    LaunchedEffect(openConversation) {
        if (openConversation > 0) settings = false
    }
    LaunchedEffect(state.updateRequired) {
        if (state.updateRequired) settings = true
    }

    Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
        BoxWithConstraints(Modifier.fillMaxSize().safeDrawingPadding().imePadding()) {
            val wide = maxWidth >= 720.dp
            BackHandler(enabled = licenses || (settings && state.workspace != null) || (!settings && !wide && selected != null)) {
                when {
                    licenses -> licenses = false
                    settings -> settings = false
                    else -> onThread(null)
                }
            }
            Column(Modifier.fillMaxSize()) {
                if (licenses) {
                    AppBar(stringResource(R.string.licenses), action = stringResource(R.string.close),
                        onAction = { licenses = false })
                    LicensesScreen()
                } else if (settings) {
                    AppBar(stringResource(R.string.connection_settings), action = if (state.workspace != null) stringResource(R.string.back_to_conversation) else null,
                        onAction = { settings = false })
                    if (attachmentView.share != null && attachments != null) SharedContentBanner(attachments::dismissShare)
                    SettingsPanel(
                        url, { url = it }, token, { token = it }, background, onBackground,
                        workerNotices, onWorkerNotices, showPlanAboveConversation, onShowPlanAboveConversation, state,
                        settingsError, { connecting = true; onConnect(url, token) },
                        { connecting = false; onDisconnect() },
                        onOpenLicenses = { licenses = true },
                        onWorkerApprovalPolicy = onWorkerApprovalPolicy,
                        onRevokeShellApproval = onRevokeShellApproval,
                        updates = {
                            UpdatePanel(updateUrl, { updateUrl = it }, updates, installError,
                                { onCheckUpdate(updateUrl) }, onDownloadUpdate, onInstallUpdate, onAllowUpdate,
                                onCancelUpdate, onAutoUpdate, onUpdateChannel, onUpdateOrigin)
                        },
                    )
                } else {
                    val timeline = selected?.let(state.timelines::get)
                    val plan = timeline?.plan
                    var planSheetOpen by rememberSaveable(state.workspace, selected) { mutableStateOf(false) }
                    val focus = LocalFocusManager.current
                    val keyboard = LocalSoftwareKeyboardController.current
                    val openPlan = {
                        focus.clearFocus()
                        keyboard?.hide()
                        planSheetOpen = true
                    }
                    val threadTitle = state.threads.find { it.id == selected }?.let { if (it.untitled) stringResource(R.string.thread_untitled) else it.title } ?: stringResource(R.string.thread)
                    AppBar(
                        title = if (!wide && selected != null) threadTitle else stringResource(R.string.app_name),
                        subtitle = if (!wide && selected != null) workspaceTitle else stringResource(state.phase.label),
                        back = if (!wide && selected != null) ({ onThread(null) }) else null,
                        action = stringResource(if (!wide && selected != null) R.string.settings else R.string.connection_settings),
                        onAction = { settings = true },
                        extraAction = {
                            if (timeline?.plan != null) PlanToggle(timeline.planAttention, openPlan)
                        },
                    )
                    state.error?.let { ErrorText(it, Modifier.padding(horizontal = 16.dp, vertical = 8.dp)) }
                    if (attachmentView.share != null && attachments != null) SharedContentBanner(attachments::dismissShare)
                    Row(Modifier.weight(1f)) {
                        if (wide || selected == null) {
                            Sidebar(
                                state, workspaceTitle, onWorkspace, { onThread(it) }, onNewThread,
                                if (wide) Modifier.width(302.dp).fillMaxHeight() else Modifier.fillMaxSize(),
                            )
                        }
                        if (wide) VerticalDivider()
                        if (selected != null) {
                            conversationState.SaveableStateProvider("${initialUrl}/${state.workspace.orEmpty()}/$selected") {
                                val target = ConversationTarget(state.workspace.orEmpty(), selected)
                                Conversation(
                                    state, selected, onSend, onStopTurn, onStopWorker, onResolve, onRememberShellApproval,
                                    Modifier.weight(1f), showPlanAboveConversation, openPlan, attachments, attachmentView, onOlderHistory, onRequestJob, onToolPayload,
                                    openApprovalDock = openApprovalDock?.takeIf { it.first == target }?.second,
                                    onDecideQueuedApprovals = { requests, accept -> onDecideQueuedApprovals(target, requests, accept) },
                                    onDismissQueuedDecision = { onDismissQueuedDecision(target) },
                                    onRetryApprovalQueue = { onRetryApprovalQueue(target) },
                                    drafts = drafts,
                                )
                            }
                        } else if (wide) {
                            Box(Modifier.weight(1f).fillMaxHeight(), contentAlignment = Alignment.Center) {
                                Text(stringResource(R.string.thread_select), color = MaterialTheme.colorScheme.onSurfaceVariant)
                            }
                        }
                    }
                    if (planSheetOpen && plan != null) PlanSheet(plan, onDismiss = { planSheetOpen = false })
                }
            }
        }
    }
}

@Composable
private fun AppBar(
    title: String, subtitle: String? = null, back: (() -> Unit)? = null,
    action: String? = null, onAction: () -> Unit = {}, extraAction: @Composable () -> Unit = {},
) {
    Surface(color = MaterialTheme.colorScheme.surface, modifier = Modifier.fillMaxWidth()) {
        Column {
            Row(
                Modifier.heightIn(min = 65.dp).padding(start = 16.dp, end = 12.dp),
                verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (back != null) TextButton(onClick = back, contentPadding = PaddingValues(horizontal = 8.dp)) {
                    Text(stringResource(R.string.back_to_threads))
                }
                Column(Modifier.weight(1f).padding(vertical = 8.dp)) {
                    Text(title, style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold,
                        maxLines = 1, overflow = TextOverflow.Ellipsis)
                    if (subtitle != null) Text(subtitle, style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
                extraAction()
                if (action != null) TextButton(onClick = onAction) {
                    Text(action, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
            HorizontalDivider()
        }
    }
}

@Composable
private fun SettingsPanel(
    url: String, onUrl: (String) -> Unit, token: String, onToken: (String) -> Unit,
    background: Boolean, onBackground: (Boolean) -> Unit,
    workerNotices: Boolean, onWorkerNotices: (Boolean) -> Unit,
    showPlanAboveConversation: Boolean, onShowPlanAboveConversation: (Boolean) -> Unit,
    state: RemoteState, settingsError: String?,
    onConnect: () -> Unit, onDisconnect: () -> Unit, onOpenLicenses: () -> Unit,
    onWorkerApprovalPolicy: (WorkerApprovalPolicy) -> Unit, onRevokeShellApproval: (String) -> Unit,
    updates: @Composable () -> Unit,
) {
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
        Column(
            Modifier.widthIn(max = 560.dp).fillMaxWidth().verticalScroll(rememberScrollState()).padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Panel {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(stringResource(R.string.server), style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                    StatusLabel(stringResource(state.phase.label))
                }
                val active = state.phase != ConnectionPhase.Disconnected
                OutlinedTextField(
                    value = url, onValueChange = onUrl, enabled = !active, singleLine = true,
                    label = { Text(stringResource(R.string.server_url)) }, placeholder = { Text("ws://your-tailnet-host.example:7878/ws") },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = token, onValueChange = onToken, enabled = !active, singleLine = true,
                    label = { Text(stringResource(R.string.token)) },
                    supportingText = { Text(stringResource(R.string.token_help)) },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                state.error?.let { ErrorText(it) }
                if (state.updateRequired) ErrorText(stringResource(R.string.update_required))
                if (active) OutlinedButton(onClick = onDisconnect, modifier = Modifier.fillMaxWidth()) {
                    Text(stringResource(R.string.disconnect))
                } else Button(onClick = onConnect, enabled = url.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
                    Text(stringResource(R.string.connect))
                }
            }
            WorkspaceApprovals(state, onWorkerApprovalPolicy, onRevokeShellApproval)
            updates()
            Panel {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text(stringResource(R.string.background_connection), style = MaterialTheme.typography.titleMedium)
                        Text(stringResource(R.string.background_connection_help),
                            style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                    Switch(checked = background, onCheckedChange = onBackground)
                }
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(stringResource(R.string.worker_notices), Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                    Switch(checked = workerNotices, onCheckedChange = onWorkerNotices, enabled = background)
                }
                settingsError?.let { ErrorText(it) }
            }
            Panel {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    val label = stringResource(R.string.plan_show_above_conversation)
                    Text(label, Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                    Switch(checked = showPlanAboveConversation, onCheckedChange = onShowPlanAboveConversation,
                        modifier = Modifier.semantics { contentDescription = label })
                }
            }
            LicensesSettingsRow(onOpenLicenses)
        }
    }
}

@Composable
private fun WorkspaceApprovals(
    state: RemoteState,
    onPolicy: (WorkerApprovalPolicy) -> Unit,
    onRevoke: (String) -> Unit,
) {
    val policy = state.workerApprovalPolicy ?: return
    Panel {
        Text(stringResource(R.string.workspace_approvals), style = MaterialTheme.typography.titleMedium)
        Text(stringResource(R.string.worker_approval_policy), style = MaterialTheme.typography.titleSmall)
        Column(Modifier.selectableGroup()) {
            for (option in WorkerApprovalPolicy.entries) {
                val label = when (option) {
                    WorkerApprovalPolicy.Ask -> R.string.worker_approval_ask
                    WorkerApprovalPolicy.Advisor -> R.string.worker_approval_advisor
                    WorkerApprovalPolicy.Auto -> R.string.worker_approval_auto
                }
                Row(
                    Modifier.fillMaxWidth().heightIn(min = 48.dp).selectable(
                        selected = policy == option, enabled = state.canSend,
                        role = Role.RadioButton, onClick = { onPolicy(option) },
                    ),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = policy == option, enabled = state.canSend, onClick = null)
                    Text(stringResource(label), Modifier.padding(start = 12.dp))
                }
            }
        }
        state.shellApprovalRules?.let { rules ->
            HorizontalDivider()
            Text(stringResource(R.string.saved_shell_approvals), style = MaterialTheme.typography.titleSmall)
            if (rules.isEmpty()) {
                Text(stringResource(R.string.no_saved_shell_approvals), style = MaterialTheme.typography.bodySmall)
            }
            for (rule in rules) {
                key(rule.id) {
                    SelectionContainer {
                        Column {
                            Text(
                                rule.prefix?.let { stringResource(R.string.shell_prefix_pattern, it) } ?: rule.command,
                                fontFamily = FontFamily.Monospace,
                            )
                            Text(rule.directory, style = MaterialTheme.typography.bodySmall)
                        }
                    }
                    OutlinedButton(
                        onClick = { onRevoke(rule.id) }, enabled = state.canSend,
                        modifier = Modifier.align(Alignment.End),
                    ) {
                        Text(stringResource(R.string.revoke_shell_approval))
                    }
                }
            }
        }
    }
}

@Composable
private fun UpdatePanel(
    url: String, onUrl: (String) -> Unit, state: UpdateState, installError: String?,
    onCheck: () -> Unit, onDownload: () -> Unit, onInstall: () -> Unit,
    onAllow: () -> Unit, onCancel: () -> Unit, onAuto: (Boolean) -> Unit,
    onChannel: (UpdateChannel) -> Unit, onOrigin: (UpdateSourcePreference) -> Unit,
) {
    Panel {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            val label = stringResource(R.string.update_auto)
            Text(label, Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
            Switch(checked = state.auto, onCheckedChange = onAuto,
                modifier = Modifier.semantics { contentDescription = label })
        }
        Text(stringResource(R.string.update_channel), style = MaterialTheme.typography.titleSmall)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (channel in UpdateChannel.entries) {
                FilterChip(state.channel == channel, { onChannel(channel) },
                    label = { Text(channel.path) }, enabled = !state.installing)
            }
        }
        var choosingSource by remember { mutableStateOf(false) }
        Box {
            val label = stringResource(state.preference.label())
            OutlinedButton(onClick = { choosingSource = true },
                enabled = !state.installing && state.channel != UpdateChannel.Dev, modifier = Modifier.fillMaxWidth()) {
                Text(stringResource(R.string.update_source, label))
            }
            DropdownMenu(choosingSource, { choosingSource = false }) {
                for (preference in UpdateSourcePreference.entries) {
                    if (preference == UpdateSourcePreference.Automatic && state.channel != UpdateChannel.Beta) continue
                    DropdownMenuItem(
                        text = { Text(stringResource(preference.label())) },
                        onClick = { choosingSource = false; onOrigin(preference) },
                    )
                }
            }
        }
        if (state.preference == UpdateSourcePreference.Custom) {
            OutlinedTextField(
                value = url, onValueChange = onUrl, enabled = !state.busy, singleLine = true,
                label = { Text(stringResource(R.string.update_url)) },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                modifier = Modifier.fillMaxWidth(),
            )
        }
        val checkedSource = when {
            state.feedOrigin == UpdateOrigin.GitHub -> R.string.update_source_github
            state.feedOrigin == UpdateOrigin.Server && state.preference == UpdateSourcePreference.Automatic ->
                R.string.update_source_server
            state.feedOrigin == UpdateOrigin.Custom && state.preference == UpdateSourcePreference.Server ->
                R.string.update_source_fallback
            else -> null
        }
        if (checkedSource != null) {
            Text(stringResource(checkedSource), style = MaterialTheme.typography.bodySmall)
        }
        OutlinedButton(onClick = onCheck, enabled = !state.busy, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.update_check))
        }
        when (val status = state.status.takeIf {
            state.preference != UpdateSourcePreference.Custom || url.trim() == state.url
        } ?: UpdateStatus.Idle) {
            UpdateStatus.Idle -> Unit
            UpdateStatus.Checking -> Text(stringResource(R.string.update_checking))
            UpdateStatus.Current -> Text(stringResource(R.string.update_current))
            is UpdateStatus.Downloading -> {
                ReleaseSummary(status.release)
                Text(stringResource(R.string.update_downloading))
                val total = status.total
                if (total == null || total == 0L) LinearProgressIndicator(Modifier.fillMaxWidth())
                else LinearProgressIndicator(progress = { status.bytes.toFloat() / total },
                    modifier = Modifier.fillMaxWidth())
            }
            is UpdateStatus.Available -> {
                ReleaseSummary(status.release)
                status.error?.let { ErrorText(stringResource(it)) }
                Button(onClick = onDownload, modifier = Modifier.fillMaxWidth()) {
                    Text(stringResource(R.string.update_download))
                }
            }
            is UpdateStatus.Ready -> {
                ReleaseSummary(status.release)
                Text(stringResource(R.string.update_install_help))
                status.error?.let { ErrorText(stringResource(it)) }
                UpdateInstallControls(state, R.string.update_install, onInstall, onAllow)
            }
            is UpdateStatus.Installing -> Text(stringResource(R.string.update_installing))
            is UpdateStatus.Confirmation -> {
                ReleaseSummary(status.release)
                Text(stringResource(R.string.update_confirmation_help))
                UpdateInstallControls(state, R.string.update_confirmation_open, onInstall, onAllow)
                TextButton(onClick = onCancel) { Text(stringResource(R.string.update_confirmation_cancel)) }
            }
            is UpdateStatus.Failed -> ErrorText(stringResource(status.error))
        }
        installError?.let { ErrorText(it) }
    }
}

private fun UpdateSourcePreference.label(): Int = when (this) {
    UpdateSourcePreference.Automatic -> R.string.update_source_automatic
    UpdateSourcePreference.Server -> R.string.update_source_server
    UpdateSourcePreference.Custom -> R.string.update_source_custom
}

@Composable
private fun ReleaseSummary(release: UpdateRelease) {
    Text(if (release.releaseNotesFirstLine.isBlank()) stringResource(R.string.update_available, release.version)
        else stringResource(R.string.update_available_notes, release.version, release.releaseNotesFirstLine))
}

@Composable
private fun UpdateInstallControls(state: UpdateState, action: Int, onInstall: () -> Unit, onAllow: () -> Unit) {
    if (!state.canInstallFromSource) {
        Text(stringResource(R.string.update_permission_help))
        OutlinedButton(onClick = onAllow, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.update_permission_open))
        }
    } else {
        if (state.applyBlocked) Text(stringResource(R.string.update_deferred))
        Button(onClick = onInstall, enabled = !state.applyBlocked, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(action))
        }
    }
}

@Composable
private fun Sidebar(
    state: RemoteState, workspaceTitle: String?, onWorkspace: (String) -> Unit,
    onThread: (String) -> Unit, onNewThread: () -> Unit, modifier: Modifier,
) {
    var chooseWorkspace by remember { mutableStateOf(false) }
    Surface(modifier, color = MaterialTheme.colorScheme.surface) {
        Column {
            Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(stringResource(R.string.workspace), style = MaterialTheme.typography.labelLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
                Box {
                    OutlinedButton(
                        { chooseWorkspace = true }, enabled = state.canSend && state.workspaces.isNotEmpty(),
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Text(workspaceTitle ?: stringResource(R.string.workspace_select), Modifier.weight(1f), maxLines = 1,
                            overflow = TextOverflow.Ellipsis)
                        Text("▾")
                    }
                    DropdownMenu(chooseWorkspace, { chooseWorkspace = false }) {
                        state.workspaces.forEach { workspace ->
                            DropdownMenuItem(
                                text = {
                                    Column {
                                        Text(workspace.title)
                                        Text(workspace.path, style = MaterialTheme.typography.bodySmall)
                                    }
                                },
                                onClick = { chooseWorkspace = false; onWorkspace(workspace.id) },
                            )
                        }
                    }
                }
            }
            HorizontalDivider()
            Row(
                Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(stringResource(R.string.threads), style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                TextButton(onClick = onNewThread, enabled = state.canSend && !state.creatingThread) {
                    Text(stringResource(R.string.thread_new_button))
                }
            }
            val listed = state.threads.mapTo(mutableSetOf()) { it.id }
            val queued = state.approvalDock.countsByThread()
            val legacy = state.approvals.values.filterNot { it.requestId in state.approvalDock.pending }
                .groupingBy { it.session }.eachCount()
            val pending = (queued.keys + legacy.keys).associateWith {
                queued.getOrDefault(it, 0) + legacy.getOrDefault(it, 0)
            }
            LazyColumn(
                Modifier.fillMaxSize(), contentPadding = PaddingValues(start = 12.dp, end = 12.dp, bottom = 12.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                items(state.threads, key = { "thread/${it.id}" }) { thread ->
                    ThreadRow(if (thread.untitled) stringResource(R.string.thread_untitled) else thread.title, thread.pinned, pending[thread.id] ?: 0,
                        state.selection == thread.id) { onThread(thread.id) }
                }
                items(pending.keys.filter { it !in listed }, key = { "pending/$it" }) { id ->
                    ThreadRow(stringResource(R.string.approval_request), false, pending.getValue(id), state.selection == id) { onThread(id) }
                }
                if (state.threads.isEmpty() && pending.isEmpty()) item {
                    Text(if (state.canSend) stringResource(R.string.threads_empty) else stringResource(state.phase.label),
                        modifier = Modifier.padding(12.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }
}

@Composable
private fun ThreadRow(title: String, pinned: Boolean, pending: Int, selected: Boolean, onClick: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    Surface(onClick = onClick, selected = selected, shape = RoundedCornerShape(12.dp),
        color = if (selected) colors.primaryContainer else colors.surface,
        contentColor = if (selected) colors.onPrimaryContainer else colors.onSurface) {
        Column(
            Modifier.fillMaxWidth().heightIn(min = 57.dp).padding(horizontal = 12.dp, vertical = 10.dp),
            verticalArrangement = Arrangement.Center,
        ) {
            Text(title.ifBlank { stringResource(R.string.thread_untitled) }, maxLines = 2, overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.bodyLarge)
            if (pinned || pending > 0) {
                val detail = listOfNotNull(stringResource(R.string.thread_pinned).takeIf { pinned }, if (pending > 0) pluralStringResource(R.plurals.approval_count, pending, pending) else null)
                Text(detail.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = colors.primary)
            }
        }
    }
}

@Composable
private fun Conversation(
    state: RemoteState, session: String, onSend: (String, String) -> Boolean, onStopTurn: (String) -> Unit,
    onStopWorker: (String, String) -> Unit, onResolve: (String, Boolean) -> Unit,
    onRememberShellApproval: (String, String?) -> Unit, modifier: Modifier,
    showPlanAboveConversation: Boolean, onOpenPlan: () -> Unit,
    attachments: Attachments?, attachmentView: AttachmentView,
    onOlderHistory: (ConversationTarget) -> Unit,
    onRequestJob: (String, String, Boolean) -> Unit,
    onToolPayload: (String, String) -> Unit,
    openApprovalDock: Int?,
    onDecideQueuedApprovals: (List<QueuedApproval>, Boolean) -> Unit, onDismissQueuedDecision: () -> Unit,
    onRetryApprovalQueue: () -> Unit,
    drafts: ConversationDrafts?,
) {
    val thread = state.threads.find { it.id == session }
    val timeline = state.timeline(session)
    val visibleRows = remember(timeline.rows, timeline.streamingSilencePrefixes, timeline.approvalReceipts) {
        timeline.displayRows.entries.toList().asReversed()
    }
    val approvals = state.approvals.values.filter {
        it.session == session && it.requestId !in state.approvalDock.pending
    }
    val queuedApprovals = remember(state.approvalDock.pending, session) {
        state.approvalDock.forThread(session)
    }
    val pending = state.pendingPrompts(session)
    val target = ConversationTarget(state.workspace.orEmpty(), session)
    val colors = MaterialTheme.colorScheme
    val plan = timeline.plan
    val listState = rememberLazyListState()
    var shownFirstPage by rememberSaveable(session) { mutableStateOf(false) }
    val requestOlder by rememberUpdatedState { onOlderHistory(target) }
    val topScroll = remember(listState) {
        object : NestedScrollConnection {
            override fun onPostScroll(consumed: Offset, available: Offset, source: NestedScrollSource): Offset {
                // Momentum also dispatches post-scroll, even when the last packet
                // lands exactly on the edge with no leftover velocity.
                if (consumed.y + available.y > 0 && !listState.canScrollForward) requestOlder()
                return Offset.Zero
            }

            override suspend fun onPostFling(consumed: Velocity, available: Velocity): Velocity {
                if (consumed.y + available.y > 0 && !listState.canScrollForward) requestOlder()
                return Velocity.Zero
            }
        }
    }
    var displayedPages by remember(listState) { mutableIntStateOf(timeline.olderPages) }
    SideEffect {
        if (!shownFirstPage && visibleRows.isNotEmpty()) {
            val newestMessage = visibleRows.indexOfFirst { it.value is TimelineRow.Message }
            if (newestMessage >= 3 && visibleRows.take(newestMessage).any {
                    (it.value as? TimelineRow.Job)?.info?.detailsDeferred == true
                }
            ) {
                listState.requestScrollToItem(newestMessage + pending.size + approvals.size)
            }
            shownFirstPage = true
        }
        if (timeline.olderPages > displayedPages) {
            // In a reversed list the highest visible index is the topmost row.
            // Keep its offset even when a page inserts rows around a sparse worker.
            val indices = visibleRows.mapIndexed { index, entry ->
                "event/${entry.key}" to (index + pending.size + approvals.size)
            }.toMap()
            val anchor = listState.layoutInfo.visibleItemsInfo.lastOrNull {
                (it.key as? String)?.let(indices::containsKey) == true
            }
            if (anchor != null) listState.requestScrollToItem(indices.getValue(anchor.key as String), -anchor.offset)
        }
        displayedPages = timeline.olderPages
    }
    Column(modifier.fillMaxHeight()) {
        if (showPlanAboveConversation && plan != null) PlanSummary(plan, onOpenPlan)
        // Measure the composer first so the dock can't crowd it out when the IME opens.
        BoxWithConstraints(Modifier.weight(1f).fillMaxWidth()) {
            val maxDockHeight = minOf(420.dp, maxHeight * 0.60f)
            Column(Modifier.fillMaxSize()) {
                Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.TopCenter) {
                    LazyColumn(
                        modifier = Modifier.widthIn(max = 760.dp).fillMaxSize().nestedScroll(topScroll),
                        state = listState, contentPadding = PaddingValues(16.dp), reverseLayout = true,
                        verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.Bottom),
                    ) {
                        items(pending.asReversed(), key = { "pending/${it.id}" }) { prompt ->
                            MessageBubble(TimelineRow.Message(prompt.text, user = true, paths = prompt.paths),
                                target, attachmentView.sent, pending = true)
                        }
                        items(approvals.asReversed(), key = { "approval/${it.requestId}" }) { request ->
                            ApprovalPanel(request, state.canSend && request.requestId !in state.resolving, onResolve,
                                onRememberShellApproval.takeIf { state.shellApprovalRules != null })
                        }
                        items(visibleRows, key = { "event/${it.key}" }) { entry ->
                            val payload = state.toolPayload?.takeIf { it.session == session }
                            val openTool: ((String) -> Unit)? = if (state.toolDetailsSupported && state.canSend) {
                                { item -> onToolPayload(session, item) }
                            } else null
                            when (val row = entry.value) {
                                is TimelineRow.Message -> MessageBubble(row, target, attachmentView.sent)
                                is TimelineRow.Job -> {
                                    val receipts = timeline.approvalReceipts.values.filter { it.workerJob == entry.key }
                                    Column {
                                        JobPanel(row, timeline.jobSummaries[entry.key] ?: row.wireSummary, payload, openTool) { older ->
                                            onRequestJob(session, entry.key, older)
                                        }
                                        for (receipt in receipts.filter { it.state == ApprovalQueueState.Denied }) {
                                            ApprovalDenialRow(receipt.tool)
                                        }
                                        if (row.details != null) {
                                            for (receipt in receipts.filter { it.automatic && it.state == ApprovalQueueState.Granted }) {
                                                Text(stringResource(R.string.tool_automatic_approval_detail, receipt.tool),
                                                    Modifier.padding(horizontal = 8.dp), style = MaterialTheme.typography.labelSmall)
                                            }
                                        }
                                    }
                                }
                                is TimelineRow.ApprovalDenial -> ApprovalDenialRow(row.tool)
                                is TimelineRow.Note -> Text(row.text, color = colors.onSurfaceVariant,
                                    style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(horizontal = 8.dp))
                                is TimelineRow.Tool -> ToolLine(row, payload, openTool)
                                is TimelineRow.ToolRun -> ToolRunPanel(row, payload, openTool)
                            }
                        }
                        if (visibleRows.isEmpty() && approvals.isEmpty() && pending.isEmpty()) item {
                            Text(stringResource(R.string.conversation_empty),
                                modifier = Modifier.padding(8.dp), color = colors.onSurfaceVariant,
                                style = MaterialTheme.typography.bodySmall)
                        }
                        if (timeline.historyEdge == HistoryEdge.Start) item(key = "history/start") {
                            Text(stringResource(R.string.conversation_start),
                                modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center,
                                style = MaterialTheme.typography.labelSmall, color = colors.onSurfaceVariant)
                        }
                    }
                    if (state.canSend && timeline.historyEdge == HistoryEdge.Loading) {
                        Text(stringResource(R.string.conversation_loading_older),
                            modifier = Modifier.background(colors.surface, RoundedCornerShape(8.dp))
                                .padding(horizontal = 8.dp, vertical = 2.dp),
                            style = MaterialTheme.typography.labelSmall, color = colors.onSurfaceVariant)
                    }
                }
                if (thread != null) {
                    if (state.hasHeldInput(session)) {
                        val label = if (state.drain?.reason == "shutdown") R.string.message_held_restart
                            else R.string.message_held_update
                        Text(stringResource(label), Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                            style = MaterialTheme.typography.labelSmall, color = colors.onSurfaceVariant)
                    }
                    ActivityLine(state, session, timeline, approvals.isNotEmpty(), pending.firstOrNull()?.sentAt, onStopWorker)
                    timeline.composerNotice?.let {
                        Text(it, Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                            style = MaterialTheme.typography.labelSmall, color = colors.onSurfaceVariant,
                            maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
                ApprovalDock(
                    queuedApprovals, state.approvalDock.decisions[session], state.canSend, maxDockHeight, openApprovalDock,
                    jobTitle = { timeline.jobSummaries[it]?.job?.displayTitle
                        ?: (timeline.rows[it] as? TimelineRow.Job)?.info?.displayTitle ?: it },
                    onDecide = onDecideQueuedApprovals,
                    onDismissResult = onDismissQueuedDecision,
                    loadError = state.approvalDock.loadError,
                    onRetry = onRetryApprovalQueue,
                )
            }
        }
        if (thread != null) {
            Composer(target, state.isTurnRunning(session), state.canSend, attachments,
                attachmentView.drafts[target] ?: AttachmentDraft(), onSend, onStopTurn, drafts)
        }
    }
}

@Composable
private fun PlanToggle(attention: PlanAttention, onOpen: () -> Unit) {
    var now by remember(attention) { mutableLongStateOf(System.nanoTime()) }
    LaunchedEffect(attention) {
        val expiry = attention.expiresAt ?: return@LaunchedEffect
        delay((expiry - System.nanoTime()).nanoseconds)
        now = expiry
    }
    val waiting = attention.isVisible(now)
    val label = stringResource(if (waiting) R.string.plan_open_waiting else R.string.plan_open)
    IconButton(onClick = onOpen, modifier = Modifier.testTag("plan-toggle").semantics { contentDescription = label }) {
        Box(Modifier.size(24.dp)) {
            val color = LocalContentColor.current
            Canvas(Modifier.size(16.dp).align(Alignment.Center)) {
                for (line in 1..3) {
                    val y = size.height * line / 4f
                    drawLine(color, Offset(1.dp.toPx(), y), Offset(size.width - 1.dp.toPx(), y),
                        strokeWidth = 2.dp.toPx(), cap = StrokeCap.Round)
                }
            }
            if (waiting) {
                Box(Modifier.align(Alignment.TopEnd).size(7.dp)
                    .background(MaterialTheme.colorScheme.tertiary, CircleShape).testTag("plan-attention"))
            }
        }
    }
}

@Composable
private fun PlanSummary(plan: Plan, onOpen: () -> Unit) {
    val (label, summary) = when (plan.status) {
        "waiting_for_user" -> stringResource(R.string.plan_waiting_for_user) to plan.question
        "active" -> stringResource(R.string.plan_next_step) to plan.next.firstOrNull()?.task
        else -> stringResource(R.string.plan) to stringResource(planStatusLabel(plan.status))
    }
    TextButton(onClick = onOpen, modifier = Modifier.fillMaxWidth().testTag("plan-summary")) {
        Text(listOfNotNull(label, summary?.takeIf(String::isNotBlank)).joinToString(" · "),
            maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PlanSheet(plan: Plan, onDismiss: () -> Unit) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        contentWindowInsets = { WindowInsets.safeDrawing },
    ) {
        Column(
            Modifier.fillMaxWidth().testTag("plan-sheet").verticalScroll(rememberScrollState())
                .padding(start = 24.dp, end = 24.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.plan), Modifier.weight(1f), style = MaterialTheme.typography.titleLarge)
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.close)) }
            }
            if (plan.goal.isNotBlank()) PlanDetail(R.string.plan_goal) { Text(plan.goal) }
            PlanDetail(R.string.plan_status) { Text(stringResource(planStatusLabel(plan.status))) }
            plan.question?.takeIf(String::isNotBlank)?.let { question ->
                PlanDetail(R.string.plan_question) { Text(question) }
            }
            if (plan.next.isNotEmpty()) PlanDetail(R.string.plan_next_steps) {
                plan.next.forEach { step ->
                    Column {
                        Text(step.task)
                        step.blocker?.reason?.takeIf(String::isNotBlank)?.let { reason ->
                            Text(reason, color = MaterialTheme.colorScheme.onSurfaceVariant,
                                style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun PlanDetail(label: Int, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text(stringResource(label), style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
        content()
    }
}

private fun planStatusLabel(status: String): Int = when (status) {
    "active" -> R.string.plan_active
    "waiting_for_user" -> R.string.plan_waiting_for_user
    "done" -> R.string.plan_criteria_met
    else -> R.string.plan_status_unknown
}

@Composable
private fun ActivityLine(
    state: RemoteState, session: String, timeline: Timeline, approvalPending: Boolean,
    firstPendingAt: Long?, onStopWorker: (String, String) -> Unit,
) {
    val jobs = remember(timeline.jobSummaries, timeline.rows) {
        val summaries = timeline.jobSummaries.active.map { Triple(it.id, it.job, it) }
        if (timeline.jobSummaries.received) summaries else summaries + timeline.rows.mapNotNull { (item, row) ->
            if (row is TimelineRow.Job && row.ok == null && timeline.jobSummaries[item] == null &&
                row.wireSummary?.status?.active != false) {
                Triple(item, row.wireSummary?.job ?: row.info, row.wireSummary)
            } else null
        }
    }
    val wait = state.waiting.reason(session)
    val drain = state.drain?.takeIf { it.holdingInput }
    val turnRunning = state.isTurnRunning(session)
    val working = (turnRunning && !approvalPending && timeline.pauseReason == null) ||
        jobs.any { (_, job) -> job.startedAt != null }
    val detail = when {
        approvalPending -> stringResource(R.string.approval_pending)
        timeline.pauseReason != null -> stringResource(R.string.reply_paused, timeline.pauseReason)
        timeline.goalProgress?.state == "paused" -> stringResource(R.string.goal_paused)
        drain?.reason == "shutdown" -> stringResource(R.string.activity_server_shutdown)
        drain != null -> stringResource(R.string.activity_server_update)
        wait != null -> stringResource(waitLabel(wait))
        firstPendingAt != null -> stringResource(R.string.message_pending)
        else -> null
    }
    if (!turnRunning && jobs.isEmpty() && detail == null) return

    val startedAt = if (working) {
        (jobs.mapNotNull { (_, job) -> job.startedAt } + listOfNotNull(timeline.runningSince)).minOrNull()
    } else if (approvalPending || timeline.pauseReason != null) {
        null
    } else drain?.startedAt ?: firstPendingAt
    val since = remember(working, wait, detail, startedAt) { startedAt ?: System.currentTimeMillis() }
    var elapsed by remember(since) { mutableLongStateOf(elapsedSince(since)) }
    LaunchedEffect(since) {
        while (true) {
            elapsed = elapsedSince(since)
            delay(1_000)
        }
    }
    var detailsOpen by rememberSaveable { mutableStateOf(false) }
    val status = stringResource(if (working) R.string.activity_working else R.string.activity_waiting)
    TextButton(onClick = { detailsOpen = true }, modifier = Modifier.padding(horizontal = 8.dp)) {
        Text("$status · ${elapsedLabel(elapsed)}", maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
    if (detailsOpen) {
        AlertDialog(
            onDismissRequest = { detailsOpen = false },
            modifier = Modifier.testTag("activity-details"),
            title = { Text(stringResource(R.string.activity_details)) },
            text = {
                Column(Modifier.heightIn(max = 320.dp).verticalScroll(rememberScrollState())) {
                    detail?.let { Text(it, style = MaterialTheme.typography.bodyMedium) }
                    if (turnRunning && detail == null) {
                        Text(stringResource(R.string.activity_front_desk), style = MaterialTheme.typography.bodyMedium)
                    }
                    jobs.forEach { (item, job, summary) ->
                        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                            Column(Modifier.weight(1f).padding(vertical = 4.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                Text(job.displayTitle, maxLines = 2, overflow = TextOverflow.Ellipsis)
                                val status = jobStatusLabel(summary?.status ?: if (job.startedAt == null) JobStatus.Queued else JobStatus.Running)
                                val age = (job.startedAt ?: job.queuedAt)?.let { at ->
                                    elapsedLabel((since + elapsed - at.coerceAtLeast(0)).coerceAtLeast(0))
                                }
                                Text(listOfNotNull(status, age).joinToString(" · "), style = MaterialTheme.typography.labelMedium)
                                JobMetadata(job, summary)
                            }
                            TextButton(onClick = { onStopWorker(session, item) }, enabled = state.canSend) {
                                Text(stringResource(R.string.stop))
                            }
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = { detailsOpen = false }) { Text(stringResource(R.string.close)) }
            },
        )
    }
}

private fun waitLabel(wait: WaitReason): Int = when (wait.kind) {
    "usage_paused" -> R.string.activity_usage_paused
    "self_update" -> R.string.activity_server_update
    "self_update_canary_approval" -> R.string.approval_pending
    "worker" -> R.string.job_queued
    "background_test" -> R.string.activity_background_test
    "background_build" -> R.string.activity_background_build
    "queued_turn" -> R.string.activity_queued_turn
    "goal" -> R.string.plan
    "watch" -> R.string.activity_watch
    else -> R.string.activity_waiting
}

@Composable
private fun MessageBubble(
    message: TimelineRow.Message, target: ConversationTarget,
    sent: Map<AttachmentLink, PreparedAttachment>, pending: Boolean = false,
) {
    val colors = MaterialTheme.colorScheme
    Box(Modifier.fillMaxWidth(), contentAlignment = if (message.user) Alignment.CenterEnd else Alignment.CenterStart) {
        Surface(
            color = if (message.user) colors.primaryContainer else colors.surface,
            contentColor = if (message.user) colors.onPrimaryContainer else colors.onSurface,
            shape = RoundedCornerShape(14.dp),
            border = if (message.user) null else BorderStroke(1.dp, colors.outlineVariant),
            modifier = Modifier.padding(
                start = if (message.user) 26.dp else 0.dp,
                end = if (message.user) 0.dp else 20.dp,
            ),
        ) {
            Column(Modifier.padding(horizontal = 14.dp, vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                val label = when {
                    pending -> R.string.message_pending
                    message.user -> R.string.message_you
                    else -> R.string.message_reply
                }
                Text(stringResource(label), style = MaterialTheme.typography.labelMedium, color = colors.primary)
                if (message.text.isNotEmpty()) {
                    if (pending) SelectionContainer { Text(message.text) } else Markdown(message.text)
                }
                SentAttachments(message.paths, target, sent)
            }
        }
    }
}

@Composable
private fun JobMetadata(info: JobInfo, summary: JobSummary?) {
    val backend = when (info.selection?.backend) {
        "claude" -> "Claude"
        "codex" -> "Codex"
        else -> info.selection?.backend
    }
    val metadata = listOfNotNull(info.model.takeIf { it.isNotBlank() }, info.selection?.effort, info.routing?.tier, backend)
    if (metadata.isNotEmpty()) StatusLabel(metadata.joinToString(" · "))
    if (summary != null) {
        Text(
            listOfNotNull(stringResource(R.string.job_tool_count, summary.tools), summary.lastActivity ?: summary.lastTool)
                .joinToString(" · "),
            maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.labelMedium,
        )
    }
}

@Composable
private fun jobStatusLabel(status: JobStatus) = stringResource(when (status) {
    JobStatus.Succeeded -> R.string.job_done
    JobStatus.Failed -> R.string.job_failed
    JobStatus.Interrupted -> R.string.job_interrupted
    JobStatus.Queued -> R.string.job_queued
    JobStatus.Running -> R.string.job_running
})

@Composable
private fun ToolLine(
    tool: TimelineRow.Tool, payload: ToolPayloadSelection?, onTool: ((String) -> Unit)?,
) {
    val status = when (tool.ok) {
        true -> stringResource(R.string.job_done_suffix)
        false -> stringResource(R.string.job_failed_suffix)
        null -> ""
    }
    val selected = payload?.takeIf { it.item == tool.item }
    val toggle = if (onTool != null && tool.item.isNotEmpty()) {
        Modifier.clickable { onTool(tool.item) }
    } else Modifier
    val disclosure = if (onTool == null) "" else if (selected == null) "▸ " else "▾ "
    Text(disclosure + tool.label + status, toggle.padding(horizontal = 8.dp, vertical = 4.dp),
        style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
    if (selected != null) {
        val detail = selected.details
        if (detail == null || detail.unavailable) {
            Text(stringResource(if (detail == null) R.string.tool_details_loading else R.string.tool_details_unavailable),
                Modifier.padding(horizontal = 8.dp), style = MaterialTheme.typography.labelSmall)
        }
        val input = (detail?.input ?: tool.input)?.toString()
        val output = detail?.output ?: tool.output
        for (text in listOfNotNull(input, output).filter(String::isNotEmpty)) {
            ToolPayloadText(text)
        }
    }
}

@Composable
private fun ToolPayloadText(text: String) {
    val chunkSize = 4096
    fun boundary(offset: Int): Int =
        if (offset in 1 until text.length && text[offset].isLowSurrogate() && text[offset - 1].isHighSurrogate()) {
            offset + 1
        } else offset
    SelectionContainer {
        LazyColumn(Modifier.fillMaxWidth().heightIn(max = 240.dp).padding(horizontal = 12.dp)) {
            items((text.length - 1) / chunkSize + 1) { index ->
                val start = boundary(index * chunkSize)
                val end = boundary((start + chunkSize - start % chunkSize).coerceAtMost(text.length))
                Text(text.substring(start, end), style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

@Composable
private fun ToolRunPanel(
    run: TimelineRow.ToolRun, payload: ToolPayloadSelection?, onTool: ((String) -> Unit)?,
) {
    val count = stringResource(R.string.tool_automatic_approval_count, run.approvalCount)
    if (run.tools.isEmpty()) {
        Text(count, Modifier.padding(horizontal = 8.dp),
            style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        return
    }
    var expanded by rememberSaveable { mutableStateOf(false) }
    Column {
        val disclosure = if (expanded) "▾" else "▸"
        val status = when {
            run.tools.any { it.first.ok == false } -> stringResource(R.string.job_failed_suffix)
            run.tools.all { it.first.ok == true } -> stringResource(R.string.job_done_suffix)
            else -> ""
        }
        Text("$disclosure " + stringResource(R.string.job_tool_count, run.tools.size) + status + " · " + count,
            Modifier.clickable { expanded = !expanded }.padding(horizontal = 8.dp, vertical = 6.dp),
            style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        if (expanded) {
            for ((tool, approvals) in run.tools) {
                ToolLine(tool, payload, onTool)
                if (approvals > 0) {
                    Text(stringResource(R.string.tool_automatic_approval_count, approvals),
                        Modifier.padding(horizontal = 16.dp), style = MaterialTheme.typography.labelSmall)
                }
            }
            if (run.unlinkedApprovals > 0) {
                Text(stringResource(R.string.tool_automatic_approval_count, run.unlinkedApprovals),
                    Modifier.padding(horizontal = 8.dp), style = MaterialTheme.typography.labelSmall)
            }
        }
    }
}

@Composable
private fun JobPanel(
    job: TimelineRow.Job, summary: JobSummary?, payload: ToolPayloadSelection?,
    onTool: ((String) -> Unit)?, onRequest: (older: Boolean) -> Unit,
) {
    val info = summary?.job ?: job.info
    val state = summary?.status ?: when (job.ok) {
        true -> JobStatus.Succeeded
        false -> JobStatus.Failed
        null -> if (info.startedAt == null) JobStatus.Queued else JobStatus.Running
    }
    val status = jobStatusLabel(state)
    val startedAt = info.startedAt ?: info.queuedAt
    var elapsed by remember(startedAt) { mutableLongStateOf(elapsedSince(startedAt)) }
    LaunchedEffect(startedAt, state) {
        while (startedAt != null && state.active) {
            elapsed = elapsedSince(startedAt)
            delay(1_000)
        }
    }
    val time = if (state.active) elapsed.takeIf { startedAt != null }
        else if (summary != null) summary.elapsed else job.report?.elapsed
    if (job.info.detailsDeferred) {
        Panel(color = MaterialTheme.colorScheme.surfaceContainer) {
            TextButton(onClick = { onRequest(false) }) {
                Text(
                    listOfNotNull(info.displayTitle, status, time?.let { elapsedLabel(it) },
                        stringResource(R.string.job_tool_count, job.info.toolCount).takeIf { summary == null }).joinToString(" · "),
                    maxLines = 1, overflow = TextOverflow.Ellipsis,
                )
            }
            JobMetadata(info, summary)
            if (job.nextDetail != null) {
                TextButton(onClick = { onRequest(true) }, enabled = !job.loadingDetails) {
                    Text(stringResource(R.string.job_earlier_activity))
                }
            }
            job.details?.forEach {
                when (it) {
                    is JobDetail.Text -> {
                        if (it.text.length > 4096) ToolPayloadText(it.text)
                        else Text(it.text, style = MaterialTheme.typography.bodyMedium)
                    }
                    is JobDetail.Tool -> {
                        ToolLine(it.row, payload, onTool)
                        if (onTool == null) {
                            val preview = listOfNotNull(it.row.input?.toString(), it.row.output).joinToString("\n")
                            if (preview.isNotEmpty()) ToolPayloadText(preview)
                        }
                    }
                }
            }
        }
        return
    }
    val detail = if (job.ok != null) job.report?.text?.takeIf { it.isNotBlank() } ?: job.summary else job.summary
    Panel(color = MaterialTheme.colorScheme.surfaceContainer) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(info.displayTitle, Modifier.weight(1f),
                style = MaterialTheme.typography.titleSmall, maxLines = 2, overflow = TextOverflow.Ellipsis)
            if (time != null) Text(elapsedLabel(time), style = MaterialTheme.typography.labelMedium)
            StatusLabel(status)
        }
        if (job.info.model.isNotBlank()) StatusLabel(job.info.model)
        if (!detail.isNullOrBlank()) Text(detail, style = MaterialTheme.typography.bodyMedium)
    }
}

private fun elapsedSince(startedAt: Long?) =
    if (startedAt == null) 0L else (System.currentTimeMillis() - startedAt.coerceAtLeast(0)).coerceAtLeast(0)

@Composable
private fun elapsedLabel(millis: Long): String {
    val seconds = millis / 1_000
    return if (seconds < 60) stringResource(R.string.elapsed_seconds, seconds) else stringResource(R.string.elapsed_minutes, seconds / 60, seconds % 60)
}

@Composable
private fun ApprovalPanel(
    request: ServerEvent.ApprovalRequested, enabled: Boolean, onResolve: (String, Boolean) -> Unit,
    onRemember: ((String, String?) -> Unit)?,
) {
    val colors = MaterialTheme.colorScheme
    var confirming by remember(request.requestId, request.shellPrefix) { mutableStateOf(false) }
    var selectedPrefix by remember(request.requestId, request.shellPrefix) { mutableStateOf<String?>(null) }
    val directory = request.shellDirectory
    val prefix = request.shellPrefix
    Panel(color = colors.surfaceContainer, border = BorderStroke(1.dp, colors.primary)) {
        Text(stringResource(R.string.approval_pending), color = colors.primary, style = MaterialTheme.typography.titleSmall)
        SelectionContainer {
            Text(request.summary, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace)
        }
        Row(Modifier.align(Alignment.End), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = { onResolve(request.requestId, false) }, enabled = enabled) { Text(stringResource(R.string.deny)) }
            Button(onClick = { onResolve(request.requestId, true) }, enabled = enabled) {
                Text(stringResource(if (request.isFolderAccess) R.string.allow_folder_one_hour else R.string.allow))
            }
        }
        if (directory != null && onRemember != null) {
            TextButton(onClick = {
                selectedPrefix = null
                confirming = true
            }, enabled = enabled, modifier = Modifier.align(Alignment.End)) {
                Text(stringResource(R.string.remember_shell_approval))
            }
            if (prefix != null) {
                TextButton(onClick = {
                    selectedPrefix = prefix
                    confirming = true
                }, enabled = enabled, modifier = Modifier.align(Alignment.End)) {
                    Text(stringResource(R.string.remember_shell_prefix_approval, prefix))
                }
            }
        }
    }
    if (confirming && directory != null && onRemember != null) {
        val chosenPrefix = selectedPrefix
        AlertDialog(
            onDismissRequest = { confirming = false },
            title = {
                Text(when {
                    chosenPrefix != null -> stringResource(R.string.remember_shell_prefix_scope, chosenPrefix)
                    prefix != null -> stringResource(R.string.remember_shell_exact_scope)
                    else -> stringResource(R.string.remember_shell_approval)
                })
            },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.verticalScroll(rememberScrollState())) {
                    val help = if (chosenPrefix == null) R.string.remember_shell_approval_help
                    else R.string.remember_shell_prefix_approval_help
                    Text(stringResource(help))
                    Text(request.summary, fontFamily = FontFamily.Monospace)
                    Text(directory, style = MaterialTheme.typography.bodySmall)
                }
            },
            confirmButton = {
                Button(enabled = enabled, onClick = {
                    confirming = false
                    onRemember(request.requestId, chosenPrefix)
                }) {
                    Text(stringResource(R.string.remember_shell_approval_confirm))
                }
            },
            dismissButton = {
                TextButton(onClick = { confirming = false }) { Text(stringResource(R.string.cancel)) }
            },
        )
    }
}

@Composable
private fun Panel(
    color: Color = MaterialTheme.colorScheme.surface,
    border: BorderStroke = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
    content: @Composable ColumnScope.() -> Unit,
) {
    Surface(Modifier.fillMaxWidth(), shape = RoundedCornerShape(16.dp), color = color, border = border) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp), content = content)
    }
}

@Composable
private fun StatusLabel(text: String) {
    Surface(color = MaterialTheme.colorScheme.primaryContainer, contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
        shape = RoundedCornerShape(20.dp)) {
        Text(text, modifier = Modifier.padding(horizontal = 10.dp, vertical = 4.dp),
            style = MaterialTheme.typography.labelMedium)
    }
}

@Composable
private fun ErrorText(text: String, modifier: Modifier = Modifier) {
    Text(text, modifier, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
}
