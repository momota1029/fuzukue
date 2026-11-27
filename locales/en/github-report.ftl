github-report-menu = Report a bug
github-report-settings = GitHub integration
github-report-connected = Connected as { $login }
github-report-disconnected = GitHub account disconnected
github-report-disconnect = Disconnect
github-report-starting = Requesting a GitHub device code…
github-report-copy-code = Copy code
github-report-repo = Repository: { $repo }
github-report-no-repo = No GitHub report repository is configured for this workspace.
github-report-preview = Review the title and body below. Sending posts them to GitHub as shown.
github-report-title = Title
github-report-title-prefix = Bug:
github-report-body = Issue body
github-report-send = Send
github-report-sending = Sending…
github-report-sent = Issue created
github-report-browser = Open in browser
github-report-long-body = The draft is too long for a browser link. Copy the missing fields into the GitHub form.
github-report-copy-body = Copy body
github-report-copy-title = Copy title
github-report-disabled = GitHub integration is unavailable on this server.
github-report-loading = Checking GitHub integration…
github-report-config-error = The server could not read its GitHub settings.
github-report-storage-error = The server could not read, save, or remove the GitHub credential.
github-report-offline = The connection was interrupted. If you were sending a report, check GitHub before retrying.
github-report-rejected = GitHub rejected the request (HTTP { $code }).
github-report-reconnect = Disconnect and reconnect GitHub.
github-report-response-error = GitHub returned an unexpected response.
github-report-denied = GitHub authorization was declined.
github-report-expired = The GitHub device code expired. Start the connection again for a new code.
github-report-cancelled = The GitHub operation was cancelled.
github-report-already-connected = A GitHub account is already connected. Refreshing account status.
github-report-not-connected = Connect your GitHub account to send from fuzukue.
github-report-server-required = Connect to the fuzukue server to send this report from the app.
github-report-close-dialog = Close the open dialog before starting a bug report.
github-report-screenshot-title = Screenshot report
github-report-screenshot-attach = Save the PNG and attach it to your issue on GitHub.
github-report-save-screenshot = Save screenshot…
github-report-screenshot-saved = Screenshot saved to { $path }
github-report-screenshot-save-error = Could not save the screenshot ({ $error }).
github-report-send-without-screenshot = Send without image
github-report-account-changed = The connected account changed. Review the new sender before sending again.
github-report-repo-denied = This repository is no longer available for bug reports from this workspace.
github-report-empty = Enter a title and body for the report.
github-report-unknown = Unknown
github-report-kind-research = Research
github-report-kind-paper = Paper
github-report-kind-software = Software
github-report-kind-empty = Empty
github-report-template =
    { "**Before posting**" } Remove credentials and private content from the report and any attachments.

    { "##" } What you did

    <!-- Steps and inputs -->

    { "##" } What happened

    <!-- What you saw, including any error message -->

    { "##" } What you expected

    <!-- How it should work -->

    { "##" } Version or revision

    <!-- Version or commit ID displayed in the app -->

    { "##" } Workspace

    <!-- Workspace kind. Omit any private names. -->

    { "##" } Logs or screenshots

    <!-- Add attachments you can share -->

    { "##" } Diagnostics

    - App: { $version }
    - Revision: { $revision }
    - OS: { $os }
    - Protocol: { $protocol }
    - Workspace kind: { $kind }

github-auth-title = GitHub authentication
github-auth-connect = Connect to GitHub…
github-auth-pending = Waiting for GitHub authorization
github-auth-countdown = Waiting · { $remaining } remaining
github-auth-chat-connected = GitHub connected as { $login }
github-auth-copy-url = Copy URL
github-auth-retry = Retry
github-auth-report = Report a bug…
github-auth-switch = Switch account…
github-auth-confirm-disconnect = Disconnect GitHub on this server?
github-auth-confirm-switch = Disconnect the current GitHub account and start connecting another?
github-auth-cancel = Cancel
github-auth-title-connected = Connected to GitHub as { $login }
github-auth-title-disconnected = GitHub is disconnected
