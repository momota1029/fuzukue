package dev.labgui.remote

import okhttp3.MediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.ResponseBody
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.Buffer
import okio.Source
import okio.Timeout
import okio.buffer
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.IOException
import java.security.MessageDigest

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class UpdateDownloadTest {
    private val context get() = RuntimeEnvironment.getApplication()
    private val source = checkNotNull(serverUpdateSource("wss://server.example/ws", UpdateChannel.Beta))
    private val bytes = "partial response that resumes"
    private val release = UpdateRelease(
        "0.2.0", BuildConfig.VERSION_CODE + 1, BuildConfig.APPLICATION_ID,
        MessageDigest.getInstance("SHA-256").digest(bytes.toByteArray()).joinToString("") { "%02x".format(it) },
        "release.apk", 18, "", "2026-11-22T08:00:00Z", 73,
    )

    @Test fun interruptedDownloadResumesWithItsValidatorAndRejectsANonApkAfterwards() {
        val requests = mutableListOf<Request>()
        val cut = 7
        val client = OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            requests += request
            if (requests.size == 1) {
                val part = Buffer().writeUtf8(bytes.take(cut))
                val broken = object : Source {
                    override fun read(sink: Buffer, byteCount: Long): Long =
                        if (part.size > 0) part.read(sink, byteCount) else throw IOException("Disconnected")
                    override fun timeout() = Timeout.NONE
                    override fun close() {}
                }.buffer()
                val body = object : ResponseBody() {
                    override fun contentLength() = bytes.length.toLong()
                    override fun contentType(): MediaType? = null
                    override fun source() = broken
                }
                response(request, 200, body).header("ETag", "\"original\"").build()
            } else {
                response(request, 206, bytes.drop(cut).toResponseBody())
                    .header("ETag", "\"original\"")
                    .header("Content-Range", "bytes $cut-${bytes.lastIndex}/${bytes.length}")
                    .build()
            }
        }.build()
        assertThrows(IOException::class.java) { UpdateDownload(context, client).download(source, release, "device") { _, _ -> } }
        val error = assertThrows(UpdateProblem::class.java) {
            UpdateDownload(context, client).download(source, release, "device") { _, _ -> }
        }
        assertEquals(R.string.update_apk_invalid, error.label)
        assertNull(requests.first().header("Range"))
        assertEquals("bytes=$cut-", requests.last().header("Range"))
        assertEquals("\"original\"", requests.last().header("If-Range"))
        assertEquals("Bearer device", requests.last().header("Authorization"))
        assertFalse(UpdateDownload(context).verifiedFile(release).exists())
    }

    @Test fun neverSendsTheDeviceTokenToACustomFeed() {
        var seen: Request? = null
        val client = OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            seen = request
            response(request, 401, "".toResponseBody()).build()
        }.build()
        val custom = UpdateSource(checkNotNull(httpsUpdateUrl("https://example.org/android-latest.json")),
            UpdateOrigin.Custom, UpdateChannel.Beta)
        val error = assertThrows(UpdateProblem::class.java) {
            UpdateDownload(context, client).check(custom, "device", null)
        }
        assertEquals(R.string.update_auth_failed, error.label)
        assertNull(checkNotNull(seen).header("Authorization"))
    }

    private fun response(request: Request, code: Int, body: ResponseBody) = Response.Builder()
        .request(request).protocol(Protocol.HTTP_1_1).code(code).message("").body(body)
}
