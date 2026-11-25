package dev.labgui.remote

import android.content.Context
import android.content.SharedPreferences
import androidx.compose.foundation.text.input.TextFieldState

internal class ConversationDrafts(context: Context, private val settings: ConnectionSettings) {
    private val prefs = context.getSharedPreferences("conversation_drafts", Context.MODE_PRIVATE)
    private val visible = mutableMapOf<String, TextFieldState>()
    val editing get() = visible.values.any { it.text.isNotEmpty() }

    fun key(target: ConversationTarget) = "${socketUrl(settings.url).orEmpty()}\n${target.workspace}\n${target.session}"
    fun read(key: String) = prefs.getString(key, "").orEmpty()

    fun show(key: String, state: TextFieldState) { visible[key] = state }
    fun hide(key: String, state: TextFieldState) {
        if (visible[key] !== state) return
        visible.remove(key)
        write(key, state.text.toString())
    }

    fun write(key: String, text: String) {
        if (text != read(key)) prefs.edit().draft(key, text).apply()
    }

    fun flush(): Boolean {
        val editor = prefs.edit()
        for ((key, state) in visible) editor.draft(key, state.text.toString())
        return editor.commit()
    }

    private fun SharedPreferences.Editor.draft(key: String, text: String) =
        if (text.isEmpty()) remove(key) else putString(key, text)
}
