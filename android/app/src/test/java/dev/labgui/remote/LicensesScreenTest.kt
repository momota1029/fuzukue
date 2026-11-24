package dev.labgui.remote

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import org.robolectric.annotation.LooperMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@LooperMode(LooperMode.Mode.PAUSED)
@Config(sdk = [36], qualifiers = "ja-rJP-w412dp-h870dp-mdpi")
class LicensesScreenTest {
    @get:Rule val compose = createComposeRule()

    @Test fun opensTheBundledLicensesFromConnectionSettings() {
        compose.setContent { RemoteTheme { RemoteApp(RemoteState()) } }
        compose.onNodeWithText("ライセンス").performScrollTo().performClick()
        val notices = hasText("Public Suffix List", substring = true)
        compose.waitUntil(timeoutMillis = 5_000) { compose.onAllNodes(notices).fetchSemanticsNodes().isNotEmpty() }
        compose.onNode(notices).assertIsDisplayed()
        compose.onRoot().captureRoboImage("build/shots/licenses.png")
        compose.onNodeWithText("閉じる").performClick()
        compose.onNodeWithText("接続設定").assertIsDisplayed()
    }
}
