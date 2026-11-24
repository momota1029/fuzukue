package dev.labgui.remote

import android.content.ClipData
import android.content.ClipDescription
import android.content.Intent
import android.net.Uri
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ShareIntentTest {
    @Test fun readsTheImageAndCaptionFromAndroidExtras() {
        val uri = Uri.parse("content://images/screenshot")
        val intent = Intent(Intent.ACTION_SEND).setType("image/png")
            .putExtra(Intent.EXTRA_STREAM, uri)
            .putExtra(Intent.EXTRA_TEXT, "See attached")
        assertEquals(SharedContent("See attached", listOf(uri.toString())), readSharedContent(intent))
    }

    @Test fun combinesMultipleStreamsAndClipData() {
        val first = Uri.parse("content://images/first")
        val second = Uri.parse("content://images/second")
        val third = Uri.parse("content://images/third")
        val intent = Intent(Intent.ACTION_SEND_MULTIPLE).setType("image/*")
            .putParcelableArrayListExtra(Intent.EXTRA_STREAM, arrayListOf(first, second))
        intent.clipData = ClipData(ClipDescription("images", arrayOf("image/*")), ClipData.Item(second))
            .apply { addItem(ClipData.Item(third)) }
        assertEquals(SharedContent(images = listOf(first, second, third).map(Uri::toString)), readSharedContent(intent))
    }
}
