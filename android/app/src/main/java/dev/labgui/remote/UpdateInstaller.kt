package dev.labgui.remote

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import java.io.File

internal class UpdateInstaller(private val context: Context) {
    private val installer get() = context.packageManager.packageInstaller
    val allowed get() = context.packageManager.canRequestPackageInstalls()

    @Suppress("DEPRECATION") // The integer flags overload also works on API 29–32.
    fun installedVersion() = context.packageManager.getPackageInfo(context.packageName, 0).longVersionCode

    fun stage(apk: File, unattended: Boolean): Int {
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName)
            setSize(apk.length())
            if (Build.VERSION.SDK_INT >= 31) {
                setRequireUserAction(if (unattended) PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED
                    else PackageInstaller.SessionParams.USER_ACTION_REQUIRED)
            }
        }
        val id = installer.createSession(params)
        try {
            installer.openSession(id).use { session ->
                session.openWrite("base.apk", 0, apk.length()).use { output ->
                    apk.inputStream().use { it.copyTo(output) }
                    session.fsync(output)
                }
            }
            return id
        } catch (error: Exception) {
            abandon(id)
            throw error
        }
    }

    fun commit(id: Int) {
        val result = Intent(context, UpdateInstallReceiver::class.java).setAction(INSTALL_RESULT)
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0)
        val sender = PendingIntent.getBroadcast(context, id, result, flags).intentSender
        installer.openSession(id).use { it.commit(sender) }
    }

    fun exists(id: Int) = installer.getSessionInfo(id) != null
    fun abandon(id: Int) { runCatching { installer.abandonSession(id) } }

    fun removeOrphanSessions(keep: Int) {
        installer.mySessions.filter { it.sessionId != keep && it.appPackageName == context.packageName }
            .forEach { abandon(it.sessionId) }
    }
}

private const val INSTALL_RESULT = "dev.fuzukue.android.UPDATE_INSTALL_RESULT"

class UpdateInstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == INSTALL_RESULT) {
            (context.applicationContext as RemoteApplication).updater.onInstallResult(intent)
        }
    }
}
