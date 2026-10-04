package dev.luksandroid

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import dev.luksandroid.session.ForegroundHost
import dev.luksandroid.session.TransferManager

/**
 * Foreground service held while transfers are queued/running or a SAF write is open (N1).
 *
 * Like [UnlockService] it does no work: it exists to lift the process out of the
 * low-memory killer's reach so a backgrounded import is not silently cut off. It is driven
 * by [dev.luksandroid.session.TransferForegroundController] through [AndroidForegroundHost]:
 * every count change re-delivers a start command, which refreshes the notification text.
 *
 * Type `dataSync` must match `android:foregroundServiceType` in the manifest or API 34+
 * throws. API 35+ caps dataSync at about six hours and then calls [onTimeout]; it must stop
 * promptly or the system raises an ANR, so it stops (see [onTimeout]).
 */
class TransferService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val count = intent?.getIntExtra(EXTRA_COUNT, 1) ?: 1
        try {
            startForeground(
                NOTIFICATION_ID,
                buildNotification(this, count),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } catch (t: Throwable) {
            // ForegroundServiceStartNotAllowedException (API 31+) and the like. The transfers do
            // not depend on this service, so survive: losing the notification must not crash
            // the process mid-copy.
            Trace.e("TransferService: startForeground refused (${Trace.throwableSummary(t)})")
            stopSelf()
            return START_NOT_STICKY
        }
        // startForegroundService() obliges startForeground() above; having done that, stop at
        // once if the work already ended (a stop that raced this start).
        if (!TransferManager.busy.value) stopSelf()
        // Not sticky: a killed process has lost the queue and any open SAF write with it.
        return START_NOT_STICKY
    }

    /**
     * dataSync foreground services are capped (about 6 h in 24 h on API 35+). At the cap the
     * system calls this and the service must stop promptly or the app is ANR'd. The transfers
     * themselves keep running, now without foreground protection: the controller does not
     * restart the service (its state has not changed), and the system refuses a dataSync start
     * after a timeout until the user brings the app to the foreground.
     */
    override fun onTimeout(startId: Int, fgsType: Int) {
        Trace.i("TransferService", "dataSync timeout, stopping")
        stopSelf()
    }

    companion object {
        const val EXTRA_COUNT = "count"
        private const val CHANNEL_ID = "transfer"
        private const val NOTIFICATION_ID = 2

        private fun buildNotification(context: Context, count: Int): Notification {
            ensureChannel(context)
            return Notification.Builder(context, CHANNEL_ID)
                .setContentTitle(context.getString(R.string.transfer_notification_title))
                .setContentText(context.getString(R.string.transfer_notification_text, count))
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setOngoing(true)
                .build()
        }

        private fun ensureChannel(context: Context) {
            val manager = context.getSystemService(NotificationManager::class.java)
            if (manager.getNotificationChannel(CHANNEL_ID) != null) return
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    context.getString(R.string.transfer_channel_name),
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = context.getString(R.string.transfer_channel_description)
                    setShowBadge(false)
                },
            )
        }
    }
}

/** [ForegroundHost] backed by [TransferService]. */
class AndroidForegroundHost(context: Context) : ForegroundHost {
    private val app = context.applicationContext

    override fun show(count: Int) {
        app.startForegroundService(
            Intent(app, TransferService::class.java).putExtra(TransferService.EXTRA_COUNT, count),
        )
    }

    override fun hide() {
        app.stopService(Intent(app, TransferService::class.java))
    }
}
