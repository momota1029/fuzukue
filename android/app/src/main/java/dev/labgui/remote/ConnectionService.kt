package dev.labgui.remote

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.IBinder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

internal const val EXTRA_WORKSPACE = "${BuildConfig.APPLICATION_ID}.workspace"
internal const val EXTRA_SESSION = "${BuildConfig.APPLICATION_ID}.session"
internal const val EXTRA_APPROVAL_DOCK = "${BuildConfig.APPLICATION_ID}.approval_dock"

class ConnectionService : Service() {
    private val app get() = application as RemoteApplication
    private val manager get() = getSystemService(NotificationManager::class.java)
    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var nextNotice = CONNECTION_ID + 1
    private val actionableNotices = mutableMapOf<Int, ApprovalNotice>()

    override fun onCreate() {
        super.onCreate()
        manager.createNotificationChannels(listOf(
            NotificationChannel(CONNECTION_CHANNEL, getString(R.string.notification_connection), NotificationManager.IMPORTANCE_LOW),
            NotificationChannel(EVENT_CHANNEL, getString(R.string.notification_events), NotificationManager.IMPORTANCE_DEFAULT),
        ))
        startForeground(
            CONNECTION_ID,
            notification(CONNECTION_CHANNEL)
                .setContentTitle(getString(R.string.app_name))
                .setContentText(getString(R.string.notification_connection_active))
                .setContentIntent(openApp(CONNECTION_ID))
                .setOngoing(true)
                .build(),
        )
        app.repository.setServiceRunning(true)
        scope.launch {
            app.repository.notices.collect { notice ->
                if (!manager.areNotificationsEnabled()) return@collect
                val id = nextNotice++
                val builder = notification(EVENT_CHANNEL)
                    .setContentTitle(notice.title)
                    .setContentText(notice.threadTitle)
                    .setAutoCancel(true)
                    .setContentIntent(openApp(id, notice))
                val approval = notice.approval
                if (approval != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    val details = app.repository.state.value.notificationApproval(approval)?.notificationDetails()
                        ?: return@collect
                    builder.setContentText(details.substringBefore('\n'))
                        .setStyle(Notification.BigTextStyle().bigText(details).setSummaryText(notice.threadTitle))
                        .addAction(approvalAction(approval, ACTION_DENY, R.string.deny))
                        .addAction(approvalAction(approval, ACTION_ALLOW, R.string.allow))
                    actionableNotices[id] = approval
                }
                manager.notify(id, builder.build())
            }
        }
        scope.launch {
            app.repository.state.collect { state ->
                actionableNotices.entries.removeAll { (id, approval) ->
                    (state.notificationApproval(approval) == null).also { if (it) manager.cancel(id) }
                }
                if (!app.repository.wantsConnection) stopSelf()
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val action = intent?.action
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && (action == ACTION_ALLOW || action == ACTION_DENY)) {
            val parts = intent?.data?.takeIf { it.scheme == "fuzukue-approval" }?.pathSegments
            if (parts?.size == 3) {
                val approval = ApprovalNotice(ConversationTarget(parts[0], parts[1]), parts[2])
                if (approval in actionableNotices.values) {
                    app.repository.resolveNotification(approval, accept = action == ACTION_ALLOW)
                }
            }
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        scope.cancel()
        actionableNotices.keys.forEach { manager.cancel(it) }
        actionableNotices.clear()
        app.repository.setServiceRunning(false)
        super.onDestroy()
    }

    private fun notification(channel: String) = Notification.Builder(this, channel)
        .setSmallIcon(R.drawable.ic_notification)
        .setVisibility(Notification.VISIBILITY_PRIVATE)

    @android.annotation.TargetApi(Build.VERSION_CODES.S)
    private fun approvalAction(approval: ApprovalNotice, action: String, label: Int): Notification.Action {
        val data = Uri.Builder().scheme("fuzukue-approval").authority(packageName)
            .appendPath(approval.target.workspace).appendPath(approval.target.session)
            .appendPath(approval.requestId).build()
        val intent = Intent(this, ConnectionService::class.java).setAction(action).setData(data)
        val pending = PendingIntent.getService(this, 0, intent, PendingIntent.FLAG_IMMUTABLE)
        return Notification.Action.Builder(null, getString(label), pending)
            .setAuthenticationRequired(true)
            .build()
    }

    private fun openApp(id: Int, notice: EventNotice? = null): PendingIntent {
        val intent = Intent(this, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        if (notice?.target != null) {
            intent.putExtra(EXTRA_WORKSPACE, notice.target.workspace)
            intent.putExtra(EXTRA_SESSION, notice.target.session)
            intent.putExtra(EXTRA_APPROVAL_DOCK, notice.openApprovalDock)
        }
        return PendingIntent.getActivity(this, id, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    }

    private companion object {
        const val CONNECTION_ID = 1
        const val CONNECTION_CHANNEL = "connection"
        const val EVENT_CHANNEL = "activity"
        const val ACTION_ALLOW = "${BuildConfig.APPLICATION_ID}.allow_approval"
        const val ACTION_DENY = "${BuildConfig.APPLICATION_ID}.deny_approval"
    }
}
