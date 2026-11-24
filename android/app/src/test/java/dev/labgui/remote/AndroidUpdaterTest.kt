package dev.labgui.remote

import android.content.Context
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class AndroidUpdaterTest {
    @Suppress("DEPRECATION") // Robolectric shadows the integer-flags overload.
    @Test fun usesTheApplicationIdForBothFileProviders() {
        val context = RuntimeEnvironment.getApplication()
        val applicationId = BuildConfig.APPLICATION_ID
        assertEquals("dev.fuzukue.android", applicationId)
        assertEquals(applicationId, context.packageName)
        val updates = updateAuthority(context)
        val camera = cameraAuthority(context)
        assertEquals("$applicationId.updates", updates)
        assertEquals("$applicationId.composer-camera", camera)
        assertEquals(
            UpdateFileProvider::class.java.name,
            context.packageManager.resolveContentProvider(updates, 0)?.name,
        )
        assertEquals(
            CameraAttachmentProvider::class.java.name,
            context.packageManager.resolveContentProvider(camera, 0)?.name,
        )
    }

    @Test fun startsWithoutAnUpdateFeed() {
        val context = RuntimeEnvironment.getApplication()
        assertEquals("", AndroidUpdater(context).state.value.url)
    }

    @Test fun usesTheStoredFeedWhenProvided() {
        val context = RuntimeEnvironment.getApplication()
        val customFeed = "https://example.org/channel/android-latest.json"
        context.getSharedPreferences("updates", Context.MODE_PRIVATE).edit().putString("url", customFeed).apply()
        assertEquals(customFeed, AndroidUpdater(context).state.value.url)
    }
}
