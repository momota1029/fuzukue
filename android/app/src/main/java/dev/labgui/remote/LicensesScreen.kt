package dev.labgui.remote

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import java.io.IOException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

@Composable
internal fun LicensesSettingsRow(onClick: () -> Unit) {
    OutlinedCard(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Text(
            stringResource(R.string.licenses),
            modifier = Modifier.padding(16.dp),
            style = MaterialTheme.typography.titleMedium,
        )
    }
}

@Composable
internal fun LicensesScreen() {
    val assets = LocalContext.current.assets
    val error = stringResource(R.string.licenses_unavailable)
    val text by produceState<String?>(null, assets, error) {
        value = withContext(Dispatchers.IO) {
            try {
                listOf("NOTICE.txt", "THIRD_PARTY_LICENSES.txt", "KaTeX-OFL-1.1.txt")
                    .joinToString("\n\n") { name ->
                        assets.open("licenses/$name").bufferedReader().use { it.readText().trimEnd() }
                    }
            } catch (_: IOException) {
                error
            }
        }
    }
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
        val content = text
        if (content == null) {
            CircularProgressIndicator(Modifier.padding(20.dp))
        } else {
            SelectionContainer(
                Modifier.widthIn(max = 720.dp).fillMaxWidth().verticalScroll(rememberScrollState()).padding(20.dp)
            ) {
                Text(content, style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
            }
        }
    }
}
