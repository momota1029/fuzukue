package dev.labgui.remote

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.Signature
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class AndroidUpdaterTest {
    private val context get() = RuntimeEnvironment.getApplication()
    private val prefs get() = context.getSharedPreferences("updates", Context.MODE_PRIVATE)
    private val settings get() = ConnectionSettings(context)

    @Before fun resetSettings() {
        prefs.edit().clear().commit()
    }

    @Test fun defaultsToTheConnectedServerAndStoresChannelAndOptIn() {
        val updater = AndroidUpdater(context)
        assertEquals(UpdateOrigin.Server, updater.state.value.origin)
        assertEquals(UpdateChannel.Beta, updater.state.value.channel)
        assertFalse(updater.state.value.auto)
        updater.setChannel(UpdateChannel.Stable)
        updater.setAuto(true)
        val restored = AndroidUpdater(context).state.value
        assertEquals(UpdateChannel.Stable, restored.channel)
        assertTrue(restored.auto)
    }

    @Test fun migratesTheExistingFeedForBetaAndLimitsDevToTheServer() {
        val customFeed = "https://example.org/channel/android-latest.json"
        prefs.edit().putString("url", customFeed).apply()
        val updater = AndroidUpdater(context)
        assertEquals(customFeed, updater.state.value.url)
        assertEquals(UpdateOrigin.Custom, updater.state.value.origin)
        updater.setChannel(UpdateChannel.Dev)
        assertEquals("", updater.state.value.url)
        assertEquals(UpdateOrigin.Server, updater.state.value.origin)
        updater.setChannel(UpdateChannel.Beta)
        assertEquals(customFeed, updater.state.value.url)
        assertEquals(UpdateOrigin.Custom, updater.state.value.origin)
    }

    @Test fun changingChannelDiscardsTheOldReadyCandidate() {
        val release = cacheCandidate()
        val updater = AndroidUpdater(context, settings)
        assertEquals(release, (updater.state.value.status as UpdateStatus.Ready).release)
        updater.setChannel(UpdateChannel.Stable)
        assertEquals(UpdateStatus.Idle, updater.state.value.status)
        assertFalse(prefs.contains("candidate"))
        assertEquals(UpdateStatus.Idle, AndroidUpdater(context, settings).state.value.status)
    }

    @Test fun resolvesAnInstallationAlreadyCompletedOnRestart() {
        prefs.edit().putInt("install_session", 42).putInt("install_version", BuildConfig.VERSION_CODE).commit()
        assertEquals(UpdateStatus.Current, AndroidUpdater(context).state.value.status)
        assertFalse(prefs.contains("install_session"))
    }

    @Test fun abandonedSessionKeepsTheCandidateAvailableForManualRetry() {
        val release = cacheCandidate()
        prefs.edit().putInt("install_session", 42).putInt("install_version", release.versionCode)
            .putBoolean("install_pending", true).commit()
        val updater = AndroidUpdater(context, settings)
        val retry = updater.state.value.status as UpdateStatus.Ready
        assertEquals(release, retry.release)
        assertEquals(R.string.update_install_failed, retry.error)
        updater.onInstallResult(Intent().putExtra(PackageInstaller.EXTRA_SESSION_ID, 42)
            .putExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_SUCCESS))
        assertEquals(retry, updater.state.value.status)
    }

    @Test fun rejectsMissingOrDifferentSigningCertificates() {
        val first = Signature(byteArrayOf(1))
        val other = Signature(byteArrayOf(2))
        assertTrue(sameApkSigners(arrayOf(first, other), arrayOf(other, first)))
        assertFalse(sameApkSigners(arrayOf(first), arrayOf(other)))
        assertFalse(sameApkSigners(arrayOf(first), null))
        assertFalse(sameApkSigners(emptyArray(), emptyArray()))
    }

    private fun cacheCandidate(): UpdateRelease {
        val release = UpdateRelease(
            "0.2.0", BuildConfig.VERSION_CODE + 1, context.packageName, "a".repeat(64),
            "app.apk", 18, "", "2026-11-22T08:00:00Z", PROTOCOL_VERSION,
        )
        val connection = "wss://example.org/ws"
        settings.save(connection, "")
        val source = checkNotNull(serverUpdateSource(connection, UpdateChannel.Beta))
        val file = UpdateDownload(context).verifiedFile(release)
        file.parentFile?.mkdirs()
        file.writeText("Already verified before process exit")
        prefs.edit().putString("candidate", Protocol.json.encodeToString(UpdateRelease.serializer(), release))
            .putString("candidate_source", "${socketUrl(connection)}\n${source.key}")
            .putString("candidate_feed", source.key).commit()
        return release
    }
}
