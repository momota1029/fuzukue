package dev.labgui.remote

import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayOutputStream

class UpdateReleaseTest {
    private val applicationId = BuildConfig.APPLICATION_ID
    private val digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    private val manifest = """
        {
          "version": "0.1.12", "versionCode": 12, "package": "$applicationId",
          "sha256": "$digest", "apk": "fuzukue-android-0.1.12.apk",
          "minimum_protocol": 18, "release_notes_first_line": "更新を追加",
          "built_at": "2026-11-22T08:00:00Z", "extra": true
        }
    """.trimIndent()

    @Test fun comparesVersionCodesNumerically() {
        val release = UpdateRelease.parse(manifest, applicationId)
        assertTrue(release.isNewerThan(9))
        assertFalse(release.isNewerThan(12))
        assertFalse(release.isNewerThan(13))
    }

    @Test fun parsesManifestAndResolvesTheApkNextToTheFeed() {
        val release = UpdateRelease.parse(manifest, applicationId)
        assertEquals("0.1.12", release.version)
        assertEquals(12, release.versionCode)
        assertEquals(applicationId, release.packageName)
        assertEquals(digest, release.sha256)
        assertEquals(18, release.minimumProtocol)
        assertEquals("更新を追加", release.releaseNotesFirstLine)
        assertEquals("2026-11-22T08:00:00Z", release.builtAt)
        val feed = checkNotNull(httpsUpdateUrl("https://example.org/channel/android-latest.json?manifest=1"))
        assertEquals("https://example.org/channel/fuzukue-android-0.1.12.apk", release.apkUrl(feed).toString())
    }

    @Test fun rejectsInvalidManifests() {
        val invalid = listOf(
            "{}",
            manifest.replace(digest, "invalid"),
            manifest.replace("fuzukue-android-0.1.12.apk", "https://example.org/update.apk"),
            manifest.replace("fuzukue-android-0.1.12.apk", "../update.apk"),
            manifest.replace("\"versionCode\": 12", "\"versionCode\": 0"),
            manifest.replace("2026-11-22T08:00:00Z", "yesterday"),
        )
        for (text in invalid) assertThrows(IllegalArgumentException::class.java) {
            UpdateRelease.parse(text, applicationId)
        }
    }

    @Test fun rejectsManifestsWithoutAMatchingPackage() {
        val packageField = ", \"package\": \"$applicationId\""
        for (text in listOf(manifest.replace(packageField, ""), manifest.replace(applicationId, "dev.example.other"))) {
            assertThrows(IllegalArgumentException::class.java) { UpdateRelease.parse(text, applicationId) }
        }
    }

    @Test fun verifiesTheSha256OfTheBytesWritten() {
        val input = "abc".toByteArray()
        val output = ByteArrayOutputStream()
        assertTrue(copyAndVerifySha256(input.inputStream(), output, digest.uppercase()))
        assertArrayEquals(input, output.toByteArray())
        assertFalse(copyAndVerifySha256("abcd".byteInputStream(), ByteArrayOutputStream(), digest))
    }

    @Test fun onlyAcceptsAbsoluteHttpsUrls() {
        assertEquals("https://example.org:8443/feed.json", httpsUpdateUrl(" HTTPS://example.org:8443/feed.json ").toString())
        for (url in listOf("", "http://example.org/feed.json", "file:///feed.json", "wss://example.org/feed.json",
            "//example.org/feed.json", "https:example.org", "https:///feed.json", "https://user@example.org/feed.json",
            "https://example.org/feed.json#fragment", "https://example.org:65536/feed.json",
        )) assertNull(url, httpsUpdateUrl(url))
    }
}
