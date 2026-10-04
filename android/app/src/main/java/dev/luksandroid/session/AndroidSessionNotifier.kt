package dev.luksandroid.session

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import dev.luksandroid.R
import dev.luksandroid.Trace

/**
 * Posts [SessionNotice]s to the notification shade. Never includes a file name or path
 * (see [SessionNotice]). Posting needs POST_NOTIFICATIONS on API 33+; without it the system
 * drops the notification, so the notice is also written to the trace by the caller.
 */
class AndroidSessionNotifier(context: Context) : SessionNotifier {
    private val app = context.applicationContext

    override fun notify(notice: SessionNotice) {
        try {
            val manager = app.getSystemService(NotificationManager::class.java)
            if (manager.getNotificationChannel(CHANNEL_ID) == null) {
                manager.createNotificationChannel(
                    NotificationChannel(
                        CHANNEL_ID,
                        app.getString(R.string.alert_channel_name),
                        NotificationManager.IMPORTANCE_HIGH,
                    ).apply { description = app.getString(R.string.alert_channel_description) },
                )
            }
            val (id, title, text) = when (notice) {
                is SessionNotice.FilesMayNotBeSaved -> Triple(
                    ID_UNSAVED,
                    app.getString(R.string.alert_files_unsaved_title),
                    if (notice.unsavedFiles > 0) {
                        app.getString(R.string.alert_files_unsaved_text_n, notice.unsavedFiles)
                    } else {
                        app.getString(R.string.alert_files_unsaved_text)
                    },
                )
                SessionNotice.SafFinishFailed -> Triple(
                    ID_SAF_FINISH,
                    app.getString(R.string.alert_saf_finish_title),
                    app.getString(R.string.alert_saf_finish_text),
                )
                SessionNotice.SafCommitFailed -> Triple(
                    ID_SAF_COMMIT,
                    app.getString(R.string.alert_saf_commit_title),
                    app.getString(R.string.alert_saf_commit_text),
                )
            }
            manager.notify(
                id,
                Notification.Builder(app, CHANNEL_ID)
                    .setContentTitle(title)
                    .setContentText(text)
                    .setStyle(Notification.BigTextStyle().bigText(text))
                    .setSmallIcon(android.R.drawable.stat_notify_error)
                    .setAutoCancel(true)
                    .build(),
            )
        } catch (t: Throwable) {
            Trace.e("AndroidSessionNotifier: could not post notice (${Trace.throwableSummary(t)})")
        }
    }

    private companion object {
        const val CHANNEL_ID = "alerts"
        const val ID_UNSAVED = 10
        const val ID_SAF_FINISH = 11
        const val ID_SAF_COMMIT = 12
    }
}
