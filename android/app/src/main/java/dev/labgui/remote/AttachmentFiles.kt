package dev.labgui.remote

import android.content.ContentResolver
import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.media.ExifInterface
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import androidx.core.content.FileProvider
import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.SequenceInputStream

class CameraAttachmentProvider : FileProvider()

internal data class AttachmentSource(
    val uri: Uri, val imageOnly: Boolean, val capture: File? = null,
    // Compose carries the IME grant in this bundle. Keep it alive until staging finishes.
    val imeGrant: Bundle? = null,
)
internal data class PreparedAttachment(val file: File?, val name: String, val size: Long, val preview: Bitmap?)
internal class AttachmentFileException(val problem: AttachmentProblem) : IOException()

internal class AttachmentFiles(private val context: Context) {
    private val cache by lazy {
        File(context.cacheDir, "attachments").apply {
            mkdirs()
            deleteStaleAttachments(this)
        }
    }

    fun accepts(uri: Uri): Boolean {
        val authority = uri.authority?.substringAfterLast('@') ?: return false
        return uri.scheme == ContentResolver.SCHEME_CONTENT && authority.isNotEmpty() &&
            authority != context.packageName && !authority.startsWith("${context.packageName}.")
    }

    fun prepare(source: AttachmentSource): PreparedAttachment {
        var staged: File? = null
        var converted: File? = null
        try {
            val resolver = context.contentResolver
            val (name, sizeHint) = if (source.capture != null) "camera.jpg" to source.capture.length()
                else resolver.query(source.uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
                    ?.use { cursor ->
                        if (!cursor.moveToFirst()) return@use null
                        val nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                        val sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE)
                        val name = if (nameIndex >= 0) cursor.getString(nameIndex) else null
                        val size = if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) cursor.getLong(sizeIndex) else null
                        name to size
                    } ?: (null to null)
            val mime = if (source.capture != null) "image/jpeg"
                else resolver.getType(source.uri)?.substringBefore(';')?.trim()?.lowercase()
            val requiresImage = source.imageOnly || mime?.startsWith("image/") == true
            if (sizeHint != null && attachmentSizing(sizeHint, requiresImage || !isDocumentAttachment(mime)) ==
                AttachmentSizing.Reject
            ) throw AttachmentFileException(AttachmentProblem.TooLarge)

            val file = source.capture ?: File.createTempFile("draft-", ".cache", cache)
            if (source.capture == null) staged = file
            val format = (source.capture?.inputStream() ?: resolver.openInputStream(source.uri)
                ?: throw AttachmentFileException(AttachmentProblem.Read)).use { input ->
                val header = ByteArray(64)
                var count = 0
                while (count < header.size) {
                    val read = input.read(header, count, header.size - count)
                    if (read == -1) break
                    count += read
                }
                val image = attachmentImage(header.copyOf(count))
                if ((requiresImage && image == null) || (image != null && !image.matchesMime(mime))) {
                    throw AttachmentFileException(AttachmentProblem.InvalidImage)
                }
                if (image == null && !isDocumentAttachment(mime)) {
                    throw AttachmentFileException(AttachmentProblem.UnsupportedFile)
                }
                if (source.capture == null) {
                    val limit = if (image == null) MAX_ATTACHMENT_BYTES else MAX_IMAGE_SOURCE_BYTES
                    val content = SequenceInputStream(ByteArrayInputStream(header, 0, count), input)
                    val fits = file.outputStream().use { copyAttachment(content, it, limit) }
                    if (!fits) throw AttachmentFileException(AttachmentProblem.TooLarge)
                }
                image
            }

            val stored: PreparedAttachment
            if (format == null) {
                stored = PreparedAttachment(file, safeAttachmentName(name), file.length(), null)
            } else {
                val size = file.length()
                val location = if (size <= MAX_ATTACHMENT_BYTES) locationExif(file, format) else null
                val sizing = if (location != null && format == AttachmentImage.Jpeg) {
                    // Keep the original pixels and orientation for in-limit JPEGs, including large photos.
                    gpsTags.forEach { location.setAttribute(it, null) }
                    location.saveAttributes()
                    attachmentSizing(file.length(), image = true)
                } else attachmentSizing(size, image = true, hasLocation = location != null)
                if (sizing == AttachmentSizing.Reject) throw AttachmentFileException(AttachmentProblem.TooLarge)
                if (sizing == AttachmentSizing.Original) {
                    stored = PreparedAttachment(file, safeAttachmentName(name, format.extension), file.length(), decode(file, 120))
                } else {
                    val output = File.createTempFile("draft-", ".cache", cache).also { converted = it }
                    val extension = convertImage(file, output, sizing)
                    stored = PreparedAttachment(output, safeAttachmentName(name, extension), output.length(), decode(output, 120))
                }
            }
            if (stored.file == staged) staged = null
            if (stored.file == converted) converted = null
            if (stored.file != source.capture) source.capture?.delete()
            return stored
        } catch (error: Exception) {
            source.capture?.delete()
            throw error
        } finally {
            staged?.delete()
            converted?.delete()
        }
    }

    private fun locationExif(file: File, format: AttachmentImage): ExifInterface? {
        if (format == AttachmentImage.Gif || format == AttachmentImage.Bmp) return null
        val exif = ExifInterface(file.absolutePath)
        return exif.takeIf {
            listOf(
                ExifInterface.TAG_GPS_LATITUDE, ExifInterface.TAG_GPS_LONGITUDE, ExifInterface.TAG_GPS_ALTITUDE,
                ExifInterface.TAG_GPS_DEST_LATITUDE, ExifInterface.TAG_GPS_DEST_LONGITUDE,
                ExifInterface.TAG_GPS_AREA_INFORMATION, ExifInterface.TAG_GPS_PROCESSING_METHOD,
            ).any { tag -> exif.getAttribute(tag) != null }
        }
    }

    private fun convertImage(input: File, output: File, sizing: AttachmentSizing): String {
        // ImageDecoder applies encoded orientation. Compressing pixels leaves the source EXIF behind.
        var bitmap = decode(input, if (sizing == AttachmentSizing.Downscale) 3500 else null)
        try {
            val alpha = bitmap.hasAlpha()
            val format = if (alpha) Bitmap.CompressFormat.PNG else Bitmap.CompressFormat.JPEG
            fun write() = output.outputStream().use {
                if (!bitmap.compress(format, 92, it)) throw AttachmentFileException(AttachmentProblem.InvalidImage)
            }
            write()
            if (output.length() > MAX_ATTACHMENT_BYTES) {
                bitmap.recycle()
                bitmap = decode(input, 3072)
                write()
            }
            if (output.length() > MAX_ATTACHMENT_BYTES) throw AttachmentFileException(AttachmentProblem.TooLarge)
            return if (alpha) "png" else "jpg"
        } finally {
            bitmap.recycle()
        }
    }

    private fun decode(file: File, maxEdge: Int?): Bitmap = try {
        ImageDecoder.decodeBitmap(ImageDecoder.createSource(file)) { decoder, info, _ ->
            val (width, height) = info.size.width to info.size.height
            if (maxEdge != null) {
                val (w, h) = fitAttachmentImage(width, height, maxEdge)
                decoder.setTargetSize(w, h)
            } else if (width.toLong() * height > 16_000_000L) {
                throw AttachmentFileException(AttachmentProblem.ImageDimensions)
            }
            decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        }
    } catch (_: ImageDecoder.DecodeException) {
        throw AttachmentFileException(AttachmentProblem.InvalidImage)
    }
}

private val gpsTags = listOf(
    "GPSVersionID", "GPSLatitudeRef", "GPSLatitude", "GPSLongitudeRef", "GPSLongitude",
    "GPSAltitudeRef", "GPSAltitude", "GPSTimeStamp", "GPSSatellites", "GPSStatus", "GPSMeasureMode",
    "GPSDOP", "GPSSpeedRef", "GPSSpeed", "GPSTrackRef", "GPSTrack", "GPSImgDirectionRef",
    "GPSImgDirection", "GPSMapDatum", "GPSDestLatitudeRef", "GPSDestLatitude", "GPSDestLongitudeRef",
    "GPSDestLongitude", "GPSDestBearingRef", "GPSDestBearing", "GPSDestDistanceRef", "GPSDestDistance",
    "GPSProcessingMethod", "GPSAreaInformation", "GPSDateStamp", "GPSDifferential", "GPSHPositioningError",
)

internal fun cameraAuthority(context: Context) = "${context.packageName}.composer-camera"

internal fun createCameraAttachment(context: Context): File =
    File.createTempFile("photo-", ".jpg", File(context.cacheDir, "composer-camera").apply {
        mkdirs()
        deleteStaleAttachments(this)
    })

private fun deleteStaleAttachments(dir: File) {
    val stale = System.currentTimeMillis() - 24 * 60 * 60 * 1000L
    dir.listFiles()?.filter { it.lastModified() < stale }?.forEach { it.delete() }
}
