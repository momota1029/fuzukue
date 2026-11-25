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
        settings.save("", "")
    }

    @Test fun defaultsToAutomaticBetaAndStoresChannelAndOptIn() {
        val updater = AndroidUpdater(context)
        assertEquals(UpdateSourcePreference.Automatic, updater.state.value.preference)
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
        assertEquals(UpdateSourcePreference.Custom, updater.state.value.preference)
        updater.setChannel(UpdateChannel.Dev)
        assertEquals("", updater.state.value.url)
        assertEquals(UpdateSourcePreference.Server, updater.state.value.preference)
        updater.setChannel(UpdateChannel.Beta)
        assertEquals(customFeed, updater.state.value.url)
        assertEquals(UpdateSourcePreference.Custom, updater.state.value.preference)
    }

    @Test fun migratesEmptyLegacyBetaUrlsAndPersistsAnExplicitServerChoice() {
        for (legacyOrigin in listOf("Server", "Custom")) {
            prefs.edit().putString("origin", legacyOrigin).putString("url_beta", "  ").commit()
            assertEquals(UpdateSourcePreference.Automatic, AndroidUpdater(context).state.value.preference)
        }
        AndroidUpdater(context).setOrigin(UpdateSourcePreference.Server)
        assertEquals(UpdateSourcePreference.Server, AndroidUpdater(context).state.value.preference)
    }

    @Test fun restoresGitHubCandidatesForHttpAndHttpsConnections() {
        val github = githubBetaSource()
        for (connection in listOf("ws://server.example/ws", "wss://server.example/ws")) {
            val primary = serverUpdateSource(connection, UpdateChannel.Beta) ?: github
            val release = cacheCandidate(connection, primary, github)
            val updater = AndroidUpdater(context, settings)
            assertEquals(release, (updater.state.value.status as UpdateStatus.Ready).release)
            assertEquals(UpdateOrigin.GitHub, updater.state.value.feedOrigin)
        }
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

    private fun cacheCandidate(
        connection: String = "wss://example.org/ws",
        source: UpdateSource = checkNotNull(serverUpdateSource(connection, UpdateChannel.Beta)),
        feed: UpdateSource = source,
    ): UpdateRelease {
        val release = UpdateRelease(
            "0.2.0", BuildConfig.VERSION_CODE + 1, context.packageName, "a".repeat(64),
            "app.apk", 18, "", "2026-11-22T08:00:00Z", PROTOCOL_VERSION,
        )
        settings.save(connection, "")
        val file = UpdateDownload(context).verifiedFile(release)
        file.parentFile?.mkdirs()
        file.writeText("Already verified before process exit")
        prefs.edit().putString("candidate", Protocol.json.encodeToString(UpdateRelease.serializer(), release))
            .putString("candidate_source", "${socketUrl(connection)}\n${source.key}")
            .putString("candidate_feed", feed.key).commit()
        return release
    }
}
