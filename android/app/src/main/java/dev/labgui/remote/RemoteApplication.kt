package dev.labgui.remote

import android.app.Application
import android.content.res.Configuration

class RemoteApplication : Application() {
    internal val settings by lazy { ConnectionSettings(this) }
    private val remote = lazy { RemoteRepository(this, settings) }
    internal val repository by remote
    internal val updater by lazy { AndroidUpdater(this, settings) }
    internal val drafts by lazy { ConversationDrafts(this, settings) }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        if (remote.isInitialized()) repository.updateLocale()
    }
}
