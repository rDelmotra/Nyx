package dev.luksandroid

import android.app.Application
import dev.luksandroid.session.AndroidSessionNotifier
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.TransferForegroundController
import dev.luksandroid.session.TransferManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import dev.luksandroid.session.LuksSessionLifecycle
import dev.luksandroid.session.UsbDetachReceiver

/**
 * Custom Application class initializing session tracking, lifecycle callbacks,
 * and USB detach broadcast listeners.
 */
class LuksApp : Application() {

    private val appScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    lateinit var sessionLifecycle: LuksSessionLifecycle
        private set

    override fun onCreate() {
        super.onCreate()
        Trace.initFileLogging(this)
        Trace.i("LuksApp", "Application initializing; LuksSession initialized")
        
        sessionLifecycle = LuksSessionLifecycle(LuksSession)
        registerActivityLifecycleCallbacks(sessionLifecycle)
        registerComponentCallbacks(sessionLifecycle)
        UsbDetachReceiver.register(this, LuksSession)

        LuksSession.notifier = AndroidSessionNotifier(this)
        // N1: one source of truth, the engine's own busy flag (queue or SAF write).
        TransferForegroundController(
            queueCount = TransferManager.queueSize,
            busy = TransferManager.busy,
            host = AndroidForegroundHost(this),
        ).start(appScope)
        // C6: fatal write errors published by the engine reach the session here, on this
        // scope, never inside the engine's lock: the teardown it triggers cancels and waits
        // for the very job that reported the error.
        appScope.launch {
            TransferManager.fatalWriteErrors.collect { LuksSession.reportWriteFailure(it) }
        }
    }

    override fun onTerminate() {
        super.onTerminate()
        UsbDetachReceiver.unregister(this)
        unregisterActivityLifecycleCallbacks(sessionLifecycle)
        unregisterComponentCallbacks(sessionLifecycle)
        appScope.cancel()
    }
}
