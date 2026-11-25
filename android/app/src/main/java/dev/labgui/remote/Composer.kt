package dev.labgui.remote

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.MediaStore
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.content.MediaType
import androidx.compose.foundation.content.consume
import androidx.compose.foundation.content.contentReceiver
import androidx.compose.foundation.content.hasMediaType
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.input.TextFieldLineLimits
import androidx.compose.foundation.text.input.rememberTextFieldState
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import kotlinx.coroutines.flow.collect
import java.io.File

@OptIn(ExperimentalFoundationApi::class)
@Composable
internal fun Composer(
    target: ConversationTarget,
    running: Boolean,
    connected: Boolean,
    attachments: Attachments?,
    attachmentDraft: AttachmentDraft,
    onSend: (String, String) -> Boolean,
    onStop: (String) -> Unit,
    drafts: ConversationDrafts? = null,
) {
    val draftKey = drafts?.key(target)
    val draft = rememberTextFieldState(initialText = if (drafts != null && draftKey != null) drafts.read(draftKey) else "")
    if (drafts != null && draftKey != null) {
        DisposableEffect(drafts, draftKey, draft) {
            drafts.show(draftKey, draft)
            onDispose { drafts.hide(draftKey, draft) }
        }
        LaunchedEffect(drafts, draftKey, draft) {
            snapshotFlow { draft.text.toString() }.collect { drafts.write(draftKey, it) }
        }
    }
    LaunchedEffect(attachmentDraft.sharedText) {
        val shared = attachments?.takeSharedText(target) ?: return@LaunchedEffect
        draft.edit {
            append(if (length == 0) shared else "\n$shared")
            selection = TextRange(length)
        }
    }
    val text = draft.text.toString()
    val items = attachmentDraft.items
    val actions = composerActions(
        running, connected, text.isNotBlank(), items.isNotEmpty(),
        isComposerCommand(text) || items.all { it.status is AttachmentStatus.Done },
    )
    val colors = MaterialTheme.colorScheme
    val placeholder = stringResource(R.string.message)
    val textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface)

    Column(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp)) {
        if (items.isNotEmpty() || actions.secondaryStop) {
            Row(Modifier.fillMaxWidth().padding(end = 3.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically) {
                Row(
                    Modifier.weight(1f).horizontalScroll(rememberScrollState()),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    items.forEach { item ->
                        key(item.id) { DraftAttachmentChip(target, item, attachments, connected) }
                    }
                }
                if (actions.secondaryStop) {
                    ComposerButton(ComposerIcon.Stop, R.string.stop, connected, colors.onSurfaceVariant,
                        background = colors.surfaceContainerHigh,
                        modifier = Modifier.testTag("composer-stop")) { onStop(target.session) }
                }
            }
        }
        attachmentDraft.notice?.let {
            Text(stringResource(it), color = colors.error, style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(start = 8.dp, bottom = 4.dp))
        }
        Surface(
            color = colors.surface,
            shape = RoundedCornerShape(26.dp),
            border = BorderStroke(1.dp, colors.outlineVariant),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Row(Modifier.fillMaxWidth().padding(horizontal = 3.dp), verticalAlignment = Alignment.Bottom) {
                AttachmentMenu(target, attachments, items.size < MAX_ATTACHMENTS)
                BasicTextField(
                    state = draft,
                    modifier = Modifier.weight(1f).padding(vertical = 12.dp)
                        .semantics { contentDescription = placeholder }
                        .contentReceiver { content ->
                            if (attachments == null || !content.hasMediaType(MediaType.Image)) {
                                return@contentReceiver content
                            }
                            val uris = mutableListOf<Uri>()
                            val remaining = content.consume { item ->
                                val uri = item.uri?.takeIf { it.scheme == "content" } ?: return@consume false
                                if (uris.size <= MAX_ATTACHMENTS) uris.add(uri)
                                true
                            }
                            attachments.addUris(target, uris, imageOnly = true,
                                imeGrant = content.platformTransferableContent?.extras)
                            remaining
                        },
                    textStyle = textStyle,
                    cursorBrush = SolidColor(colors.primary),
                    lineLimits = TextFieldLineLimits.MultiLine(maxHeightInLines = 5),
                    decorator = { inner ->
                        Box {
                            if (draft.text.isEmpty()) {
                                Text(placeholder, color = colors.onSurfaceVariant, style = textStyle)
                            }
                            inner()
                        }
                    },
                )
                if (actions.primaryStop) {
                    ComposerButton(ComposerIcon.Stop, R.string.stop, connected, colors.onSurfaceVariant,
                        background = colors.surfaceContainerHigh,
                        modifier = Modifier.testTag("composer-stop")) { onStop(target.session) }
                } else {
                    ComposerButton(
                        ComposerIcon.Send, R.string.send, actions.canSend,
                        if (actions.canSend) colors.onPrimary else colors.onSurfaceVariant,
                        background = if (actions.canSend) colors.primary else colors.surfaceContainerHigh,
                        modifier = Modifier.testTag("composer-send"),
                    ) {
                        if (onSend(target.session, text)) draft.edit { replace(0, length, "") }
                    }
                }
            }
        }
    }
}

@Composable
private fun AttachmentMenu(target: ConversationTarget, attachments: Attachments?, hasRoom: Boolean) {
    val context = LocalContext.current
    var open by remember { mutableStateOf(false) }
    var captureName by rememberSaveable { mutableStateOf<String?>(null) }
    val photo = rememberLauncherForActivityResult(remember { ActivityResultContracts.PickMultipleVisualMedia(MAX_ATTACHMENTS) }) {
        attachments?.addUris(target, it, imageOnly = true)
    }
    val documents = rememberLauncherForActivityResult(remember { ActivityResultContracts.OpenMultipleDocuments() }) {
        attachments?.addUris(target, it, imageOnly = false)
    }
    val camera = rememberLauncherForActivityResult(remember { TakeAttachmentPicture() }) { saved ->
        val file = captureName?.let { File(context.cacheDir, "composer-camera/$it") }
        captureName = null
        if (file != null) {
            if (saved && attachments != null) {
                val uri = FileProvider.getUriForFile(context, cameraAuthority(context), file)
                attachments.addCapture(target, AttachmentSource(uri, imageOnly = true, capture = file))
            } else file.delete()
        }
    }
    val cameraAvailable = remember(context) {
        Intent(MediaStore.ACTION_IMAGE_CAPTURE).resolveActivity(context.packageManager) != null
    }
    Box {
        ComposerButton(ComposerIcon.Add, R.string.attachment_add, attachments != null && hasRoom,
            MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.testTag("composer-attach")) {
            open = true
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(text = { Text(stringResource(R.string.attachment_choose_images)) }, onClick = {
                open = false
                try {
                    photo.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
                } catch (_: ActivityNotFoundException) {
                    attachments?.notice(target, R.string.attachment_picker_unavailable)
                }
            })
            DropdownMenuItem(text = { Text(stringResource(R.string.attachment_choose_files)) }, onClick = {
                open = false
                try {
                    documents.launch(documentAttachmentMimes)
                } catch (_: ActivityNotFoundException) {
                    attachments?.notice(target, R.string.attachment_picker_unavailable)
                }
            })
            if (cameraAvailable) {
                DropdownMenuItem(text = { Text(stringResource(R.string.attachment_camera)) }, onClick = {
                    open = false
                    var created: File? = null
                    try {
                        val file = createCameraAttachment(context).also { created = it }
                        captureName = file.name
                        camera.launch(FileProvider.getUriForFile(context, cameraAuthority(context), file))
                    } catch (_: Exception) {
                        captureName = null
                        created?.delete()
                        attachments?.notice(target, R.string.attachment_camera_unavailable)
                    }
                })
            }
        }
    }
}

private class TakeAttachmentPicture : ActivityResultContracts.TakePicture() {
    override fun createIntent(context: Context, input: Uri): Intent =
        super.createIntent(context, input).apply {
            clipData = ClipData.newRawUri("", input)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        }
}

@Composable
private fun DraftAttachmentChip(
    target: ConversationTarget, item: DraftAttachment, attachments: Attachments?, connected: Boolean,
) {
    val colors = MaterialTheme.colorScheme
    val preview = item.prepared
    val name = preview?.name ?: stringResource(R.string.attachment_file)
    val status = item.status
    Surface(shape = RoundedCornerShape(12.dp), color = colors.surfaceContainer) {
        Row(Modifier.padding(start = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            AttachmentPreview(preview)
            Column(Modifier.widthIn(max = 150.dp).padding(start = 6.dp, top = 4.dp, bottom = 4.dp)) {
                Text(name, style = MaterialTheme.typography.labelMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (preview != null) AttachmentSize(preview.size)
                when (status) {
                    AttachmentStatus.Pending -> Text(
                        stringResource(R.string.attachment_preparing), style = MaterialTheme.typography.labelSmall,
                    )
                    is AttachmentStatus.Uploading -> LinearProgressIndicator(
                        progress = {
                            if (status.cursor.size == 0L) 0f else status.cursor.offset.toFloat() / status.cursor.size
                        },
                        modifier = Modifier.width(76.dp).padding(top = 2.dp),
                    )
                    is AttachmentStatus.Failed -> Text(
                        stringResource(status.problem.label), color = colors.error, style = MaterialTheme.typography.labelSmall,
                    )
                    is AttachmentStatus.Done -> Unit
                }
            }
            if (item.canRetry) {
                ComposerButton(ComposerIcon.Retry, R.string.attachment_retry, connected, colors.onSurfaceVariant) {
                    attachments?.retry(target, item.id)
                }
            }
            ComposerButton(ComposerIcon.Remove, R.string.attachment_remove, true, colors.onSurfaceVariant) {
                attachments?.remove(target, item.id)
            }
        }
    }
}

@Composable
internal fun SentAttachments(
    paths: List<String>, target: ConversationTarget, sent: Map<AttachmentLink, PreparedAttachment>,
) {
    if (paths.isEmpty()) return
    Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        paths.forEach { path ->
            val local = sent[AttachmentLink(target, path)]
            Surface(shape = RoundedCornerShape(8.dp), color = MaterialTheme.colorScheme.surfaceContainer) {
                Row(Modifier.padding(5.dp), verticalAlignment = Alignment.CenterVertically) {
                    AttachmentPreview(local)
                    Column(Modifier.widthIn(max = 150.dp).padding(horizontal = 5.dp)) {
                        Text(local?.name ?: path.substringAfterLast('/'), style = MaterialTheme.typography.labelMedium,
                            maxLines = 1, overflow = TextOverflow.Ellipsis)
                        if (local != null) AttachmentSize(local.size)
                    }
                }
            }
        }
    }
}

@Composable
internal fun SharedContentBanner(onDismiss: () -> Unit) {
    Row(Modifier.fillMaxWidth().padding(start = 16.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(stringResource(R.string.attachment_share_thread), Modifier.weight(1f),
            style = MaterialTheme.typography.bodySmall)
        ComposerButton(ComposerIcon.Remove, R.string.attachment_share_dismiss, true,
            MaterialTheme.colorScheme.onSurfaceVariant, onClick = onDismiss)
    }
}

@Composable
private fun AttachmentSize(size: Long) {
    val context = LocalContext.current
    val formatted = remember(context, size) { android.text.format.Formatter.formatShortFileSize(context, size) }
    Text(formatted, style = MaterialTheme.typography.labelSmall)
}

@Composable
private fun AttachmentPreview(file: PreparedAttachment?) {
    val preview = file?.preview
    if (preview != null) {
        Image(remember(preview) { preview.asImageBitmap() }, contentDescription = null,
            modifier = Modifier.size(36.dp).clip(RoundedCornerShape(5.dp)), contentScale = ContentScale.Crop)
    } else {
        Box(Modifier.size(36.dp), contentAlignment = Alignment.Center) {
            DrawComposerIcon(ComposerIcon.File, MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

private val AttachmentProblem.label: Int
    get() = when (this) {
        AttachmentProblem.TooLarge -> R.string.attachment_too_large
        AttachmentProblem.ImageDimensions -> R.string.attachment_image_dimensions
        AttachmentProblem.InvalidImage -> R.string.attachment_invalid_image
        AttachmentProblem.UnsupportedFile -> R.string.attachment_unsupported_file
        AttachmentProblem.Read -> R.string.attachment_read_error
        AttachmentProblem.Disconnected -> R.string.attachment_disconnected
        AttachmentProblem.Upload -> R.string.attachment_upload_error
    }

private enum class ComposerIcon { Add, Send, Stop, Remove, Retry, File }

@Composable
private fun ComposerButton(
    icon: ComposerIcon,
    label: Int,
    enabled: Boolean,
    color: Color,
    modifier: Modifier = Modifier,
    background: Color = Color.Transparent,
    onClick: () -> Unit,
) {
    val description = stringResource(label)
    Box(
        modifier.size(48.dp).clip(CircleShape)
            .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
            .semantics { contentDescription = description },
        contentAlignment = Alignment.Center,
    ) {
        Box(Modifier.size(40.dp).background(background, CircleShape), contentAlignment = Alignment.Center) {
            DrawComposerIcon(icon, if (enabled) color else color.copy(alpha = 0.48f))
        }
    }
}

@Composable
private fun DrawComposerIcon(icon: ComposerIcon, color: Color) {
    Canvas(Modifier.size(20.dp)) {
        val unit = size.width / 20f
        fun line(x: Int, y: Int, toX: Int, toY: Int) {
            drawLine(color, Offset(x * unit, y * unit), Offset(toX * unit, toY * unit),
                strokeWidth = 2.dp.toPx(), cap = StrokeCap.Round)
        }
        when (icon) {
            ComposerIcon.Add -> {
                line(4, 10, 16, 10)
                line(10, 4, 10, 16)
            }
            ComposerIcon.Send -> {
                line(10, 4, 10, 16)
                line(4, 10, 10, 4)
                line(10, 4, 16, 10)
            }
            ComposerIcon.Stop -> drawRoundRect(color, Offset(4 * unit, 4 * unit), Size(12 * unit, 12 * unit),
                CornerRadius(2 * unit))
            ComposerIcon.Remove -> {
                line(5, 5, 15, 15)
                line(15, 5, 5, 15)
            }
            ComposerIcon.Retry -> {
                drawArc(color, -30f, 305f, false, Offset(4 * unit, 4 * unit), Size(12 * unit, 12 * unit),
                    style = androidx.compose.ui.graphics.drawscope.Stroke(2.dp.toPx(), cap = StrokeCap.Round))
                line(11, 8, 16, 8)
                line(16, 3, 16, 8)
            }
            ComposerIcon.File -> {
                line(5, 3, 11, 3)
                line(11, 3, 15, 7)
                line(15, 7, 15, 17)
                line(15, 17, 5, 17)
                line(5, 17, 5, 3)
                line(11, 3, 11, 7)
                line(11, 7, 15, 7)
            }
        }
    }
}
