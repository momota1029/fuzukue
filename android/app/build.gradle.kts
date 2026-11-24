import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.roborazzi)
}

val androidVersion = Properties().apply {
    rootProject.file("version.properties").inputStream().use { load(it) }
}
val keystore = Properties().apply {
    val propertiesFile = rootProject.file("keystore.properties")
    if (propertiesFile.isFile) propertiesFile.inputStream().use { load(it) }
}
fun signingValue(environmentName: String, propertyName: String): String? =
    providers.environmentVariable(environmentName).orNull?.takeIf { it.isNotEmpty() }
        ?: keystore.getProperty(propertyName)?.takeIf { it.isNotEmpty() }

val releaseStore = signingValue("FUZUKUE_ANDROID_KEYSTORE", "storeFile")
val releaseStorePassword = signingValue("FUZUKUE_ANDROID_STORE_PASSWORD", "storePassword")
val releaseKeyAlias = signingValue("FUZUKUE_ANDROID_KEY_ALIAS", "keyAlias")
val releaseKeyPassword = signingValue("FUZUKUE_ANDROID_KEY_PASSWORD", "keyPassword")
val hasReleaseSigning = listOf(releaseStore, releaseStorePassword, releaseKeyAlias, releaseKeyPassword)
    .all { it != null }

android {
    namespace = "dev.labgui.remote"
    compileSdk { version = release(37) }
    defaultConfig {
        applicationId = "dev.fuzukue.android"
        minSdk = 29
        targetSdk = 36
        versionCode = androidVersion.getProperty("versionCode").toInt()
        versionName = androidVersion.getProperty("versionName")
    }
    signingConfigs {
        if (hasReleaseSigning) create("release") {
            storeFile = releaseStore?.let { rootProject.file(it) }
            storePassword = releaseStorePassword
            keyAlias = releaseKeyAlias
            keyPassword = releaseKeyPassword
        }
    }
    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.findByName("release")
            optimization {
                // Verify code and resource shrinking on a device before enabling them for release.
                enable = false
            }
        }
    }
    buildFeatures {
        buildConfig = true
        compose = true
    }
    testOptions { unitTests { isIncludeAndroidResources = true } }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.activity.compose)
    implementation(libs.okhttp)
    implementation(libs.ratex)
    implementation(libs.serialization.json)
    testImplementation(platform(libs.compose.bom))
    testImplementation(libs.junit)
    testImplementation(libs.robolectric)
    testImplementation(libs.roborazzi)
    testImplementation(libs.roborazzi.compose)
    testImplementation(libs.compose.ui.test.junit4)
    debugImplementation(libs.compose.ui.test.manifest)
}
