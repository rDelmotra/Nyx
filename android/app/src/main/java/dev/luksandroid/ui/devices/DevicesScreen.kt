package dev.luksandroid.ui.devices

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbManager
import android.os.Build
import android.text.Editable
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.BugReport
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ErrorOutline
import androidx.compose.material.icons.filled.Fingerprint
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.Key
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.LockOpen
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Security
import androidx.compose.material.icons.filled.Storage
import androidx.compose.material.icons.filled.Usb
import androidx.compose.material.icons.filled.UsbOff
import androidx.compose.material.icons.filled.VpnKey
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import dev.luksandroid.LuksDevice
import dev.luksandroid.LuksException
import dev.luksandroid.PartitionInfo
import dev.luksandroid.Trace
import dev.luksandroid.UsbMassStorage
import dev.luksandroid.biometricKeyFor
import dev.luksandroid.formatSize
import dev.luksandroid.security.BiometricPassphraseVault
import dev.luksandroid.security.PassphraseScrubber
import dev.luksandroid.security.SecurePassphraseBuffer
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionState
import dev.luksandroid.ui.LuksHaptics
import dev.luksandroid.ui.SecurePassphraseField
import dev.luksandroid.ui.components.DiskVisualMap
import dev.luksandroid.ui.theme.DarkOutline
import dev.luksandroid.ui.theme.DestructiveRed
import dev.luksandroid.ui.theme.StateLocked
import dev.luksandroid.ui.theme.StateUnlocked
import dev.luksandroid.ui.theme.WarningAmber
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * State of a discovered USB target device before unlocking.
 */
private sealed interface DeviceItemState {
    data object Idle : DeviceItemState
    data object Opening : DeviceItemState
    data class Opened(val device: LuksDevice) : DeviceItemState
    data class Failed(val error: String) : DeviceItemState
}

@Composable
fun DevicesScreen(
    modifier: Modifier = Modifier,
    onNavigateToBrowser: (() -> Unit)? = null,
    onOpenDiagnostics: (() -> Unit)? = null,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val sessionState by LuksSession.state.collectAsState()
    val activity = remember(context) { context.findFragmentActivity() }

    var targets by remember { mutableStateOf<List<UsbMassStorage.Target>>(emptyList()) }
    var deviceStates by remember { mutableStateOf<Map<String, DeviceItemState>>(emptyMap()) }
    var isScanning by remember { mutableStateOf(false) }

    // Bumped whenever enrollment state might have changed underneath us: after
    // enrolling/forgetting, and on ON_RESUME so a fingerprint added or removed
    // in system Settings while this screen was backgrounded is picked up.
    var vaultVersion by remember { mutableIntStateOf(0) }
    val biometricAvailable = remember(activity, vaultVersion) {
        activity != null && BiometricPassphraseVault.isAvailable(context)
    }
    var biometricBusy by remember { mutableStateOf(false) }
    var dialogNotice by remember { mutableStateOf<String?>(null) }
    var forgetTarget by remember { mutableStateOf<Pair<String, PartitionInfo>?>(null) }

    DisposableEffect(activity) {
        if (activity == null) return@DisposableEffect onDispose {}
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) vaultVersion++
        }
        activity.lifecycle.addObserver(observer)
        onDispose { activity.lifecycle.removeObserver(observer) }
    }

    fun keyOf(target: UsbMassStorage.Target) =
        "${target.device.deviceId}:${target.device.vendorId}:${target.device.productId}:${target.usbInterface.id}"

    // Re-opens a target whose USB permission is already granted, marking it Opening in
    // the meantime. Shared by the initial scan and by stale-handle recovery below, so
    // both paths reopen a device the exact same way.
    fun reopenTarget(target: UsbMassStorage.Target) {
        val key = keyOf(target)
        deviceStates = deviceStates + (key to DeviceItemState.Opening)
        scope.launch {
            val opened = openTarget(context, target)
            deviceStates = deviceStates + (key to opened)
        }
    }

    fun scanDevices() {
        isScanning = true
        scope.launch {
            val list = withContext(Dispatchers.IO) {
                UsbMassStorage.findTargets(context)
            }
            targets = list
            val sessionDev = LuksSession.device as? LuksDevice
            // Clean up stale device entries for devices no longer present or already closed
            val validKeys = list.map { keyOf(it) }.toSet()
            deviceStates.forEach { (k, v) ->
                if (v is DeviceItemState.Opened && (k !in validKeys || !v.device.isOpen)) {
                    if (v.device !== sessionDev) {
                        runCatching { v.device.close() }
                    }
                }
            }
            deviceStates = deviceStates.filter { (k, v) ->
                k in validKeys && (v !is DeviceItemState.Opened || v.device.isOpen)
            }
            // Auto-open devices that already have permissions granted, respecting active session
            list.forEach { target ->
                val key = keyOf(target)
                val activeUsb = LuksSession.activeUsbDevice
                val isSessionTarget = activeUsb != null && (
                    target.device.deviceId == activeUsb.deviceId ||
                    (target.device.vendorId == activeUsb.vendorId && target.device.productId == activeUsb.productId)
                )

                if (isSessionTarget && (LuksSession.state.value is SessionState.Unlocked || LuksSession.state.value is SessionState.Unlocking)) {
                    if (sessionDev != null && sessionDev.isOpen) {
                        deviceStates = deviceStates + (key to DeviceItemState.Opened(sessionDev))
                    } else if (LuksSession.state.value is SessionState.Unlocking) {
                        deviceStates = deviceStates + (key to DeviceItemState.Opening)
                    }
                } else if (UsbMassStorage.hasPermission(context, target.device) &&
                    deviceStates[key] !is DeviceItemState.Opened &&
                    deviceStates[key] !is DeviceItemState.Opening
                ) {
                    reopenTarget(target)
                }
            }
            isScanning = false
        }
    }

    LaunchedEffect(Unit) {
        scanDevices()
    }

    DisposableEffect(context) {
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context, intent: Intent) {
                val action = intent.action
                if (action == UsbManager.ACTION_USB_DEVICE_ATTACHED ||
                    action == UsbManager.ACTION_USB_DEVICE_DETACHED
                ) {
                    scanDevices()
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }
        onDispose {
            runCatching { context.unregisterReceiver(receiver) }
            val sessionDev = LuksSession.device as? LuksDevice
            deviceStates.values.forEach { state ->
                if (state is DeviceItemState.Opened && state.device !== sessionDev) {
                    runCatching { state.device.close() }
                }
            }
        }
    }

    // A lock — automatic (idle timeout, trim) or manual (the Lock button) — tears down
    // the exact LuksDevice instance the UI is still holding in `deviceStates`: closing it
    // zeroes the native handle AND releases the USB interface underneath it. Without this,
    // the cached `Opened(device)` entry looks fine to the UI but every native call on it
    // (including a fresh unlock attempt) throws `IllegalStateException` instantly, and the
    // only way out was force-stopping the app. Once the session reports Locked or Detached, drop any
    // device entries whose handle is no longer open and transparently re-acquire them.
    LaunchedEffect(sessionState) {
        if (sessionState is SessionState.Locked || sessionState is SessionState.Detached) {
            val staleKeys = deviceStates.filterValues {
                it is DeviceItemState.Opened && !it.device.isOpen
            }.keys
            if (staleKeys.isNotEmpty()) {
                deviceStates = deviceStates - staleKeys
            }
            scanDevices()
        }
    }

    // In-place unlock state for passphrase entry
    var promptDialogTarget by remember { mutableStateOf<Pair<LuksDevice, PartitionInfo>?>(null) }

    LaunchedEffect(deviceStates) {
        if (promptDialogTarget == null) {
            val firstLuks = deviceStates.values
                .filterIsInstance<DeviceItemState.Opened>()
                .map { it.device }
                .firstNotNullOfOrNull { dev ->
                    dev.openablePartitions.firstOrNull { it.isLuks }?.let { dev to it }
                }
            if (firstLuks != null) {
                promptDialogTarget = firstLuks
            }
        }
    }

    fun startManualUnlock(device: LuksDevice, partition: PartitionInfo, notice: String? = null) {
        dialogNotice = notice
        promptDialogTarget = device to partition
    }

    fun submitManualUnlock(
        device: LuksDevice,
        partition: PartitionInfo,
        buffer: SecurePassphraseBuffer,
        enrollBiometric: Boolean,
    ) {
        val bioKey = if (enrollBiometric) device.biometricKeyFor(partition) else null
        val act = activity
        LuksSession.scope.launch {
            buffer.use { buf ->
                var pendingKey: String? = null
                if (bioKey != null && act != null) {
                    withContext(Dispatchers.Main) { biometricBusy = true }
                    try {
                        if (enrollBiometric(act, bioKey, partition.name.ifBlank { "Partition #${partition.index}" }, buf)) {
                            pendingKey = bioKey
                        }
                    } finally {
                        withContext(Dispatchers.Main) { biometricBusy = false }
                    }
                }
                var unlocked = false
                try {
                    val res = LuksSession.unlock(context, device, partition, buf)
                    unlocked = res is SessionState.Unlocked
                    if (res is SessionState.Failed) {
                        withContext(Dispatchers.Main) {
                            dialogNotice = res.message
                        }
                    } else if (unlocked) {
                        withContext(Dispatchers.Main) {
                            promptDialogTarget = null
                            dialogNotice = null
                        }
                    }
                } finally {
                    pendingKey?.let { k ->
                        if (!unlocked) BiometricPassphraseVault.forget(context, k)
                        withContext(Dispatchers.Main) { vaultVersion++ }
                    }
                }
            }
        }
    }

    fun startBiometricUnlock(device: LuksDevice, partition: PartitionInfo) {
        val act = activity ?: return
        val bioKey = device.biometricKeyFor(partition) ?: return
        if (biometricBusy) return
        val label = partition.name.ifBlank { "Partition #${partition.index}" }
        biometricBusy = true
        scope.launch {
            val outcome = try {
                obtainPassphraseWithBiometric(act, bioKey, label)
            } finally {
                biometricBusy = false
            }
            when (outcome) {
                is BiometricUnlockOutcome.UseManual -> {
                    vaultVersion++
                    startManualUnlock(device, partition, outcome.notice)
                }
                is BiometricUnlockOutcome.Passphrase -> {
                    LuksSession.scope.launch {
                        outcome.buffer.use { buf ->
                            val result = LuksSession.unlock(context, device, partition, buf)
                            if (result is SessionState.Failed && result.message.contains("wrong passphrase", ignoreCase = true)) {
                                BiometricPassphraseVault.forget(context, bioKey)
                                withContext(Dispatchers.Main) {
                                    vaultVersion++
                                    dialogNotice = BIOMETRIC_NOTICE_STALE
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    val onRetryPartition: (LuksDevice, PartitionInfo) -> Unit = { dev, part ->
        scope.launch {
            LuksSession.reset()
            if (part.isPlain) {
                LuksSession.openPlain(context, dev, part)
            } else {
                promptDialogTarget = dev to part
            }
        }
    }
    val onResetSession: () -> Unit = {
        scope.launch {
            LuksSession.reset()
            scanDevices()
        }
    }

    if (targets.isEmpty() && sessionState !is SessionState.Unlocked) {
        Column(
            modifier = modifier
                .fillMaxSize()
                .padding(16.dp),
        ) {
            QuietRoomHeader(
                isScanning = isScanning,
                onScan = { scanDevices() },
                onOpenDiagnostics = onOpenDiagnostics,
            )

            GlobalSessionNotices(
                sessionState = sessionState,
                deviceStates = deviceStates,
                onRetryPartition = onRetryPartition,
                onReset = onResetSession,
                modifier = Modifier.padding(top = 16.dp),
            )

            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth(),
                contentAlignment = Alignment.Center,
            ) {
                EmptyDevicesView(onScan = { scanDevices() }, isScanning = isScanning)
            }

            QuietSecurityFooter()
        }
    } else {
        Column(
            modifier = modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            QuietRoomHeader(
                isScanning = isScanning,
                onScan = { scanDevices() },
                onOpenDiagnostics = onOpenDiagnostics,
            )

            GlobalSessionNotices(
                sessionState = sessionState,
                deviceStates = deviceStates,
                onRetryPartition = onRetryPartition,
                onReset = onResetSession,
            )

            targets.forEach { target ->
                val key = keyOf(target)
                val itemState = deviceStates[key] ?: DeviceItemState.Idle
                val hasPerm = UsbMassStorage.hasPermission(context, target.device)

                TargetDeviceCard(
                    target = target,
                    hasPermission = hasPerm,
                    itemState = itemState,
                    sessionState = sessionState,
                    biometricAvailable = biometricAvailable,
                    biometricBusy = biometricBusy,
                    vaultVersion = vaultVersion,
                    promptDialogTarget = promptDialogTarget,
                    dialogNotice = dialogNotice,
                    onRequestPermission = {
                        scope.launch {
                            val granted = UsbMassStorage.requestPermission(context, target.device)
                            if (granted) {
                                deviceStates = deviceStates + (key to DeviceItemState.Opening)
                                val opened = openTarget(context, target)
                                deviceStates = deviceStates + (key to opened)
                            }
                        }
                    },
                    onOpen = {
                        deviceStates = deviceStates + (key to DeviceItemState.Opening)
                        scope.launch {
                            val opened = openTarget(context, target)
                            deviceStates = deviceStates + (key to opened)
                        }
                    },
                    onUnlockPartition = { device, partition ->
                        startManualUnlock(device, partition)
                    },
                    onBiometricUnlockPartition = { device, partition ->
                        startBiometricUnlock(device, partition)
                    },
                    onMountPartition = { device, partition ->
                        LuksSession.scope.launch {
                            LuksSession.openPlain(context, device, partition)
                        }
                    },
                    onForgetBiometric = { bioKey, partition ->
                        forgetTarget = bioKey to partition
                    },
                    onSubmitUnlock = { device, partition, buffer, enroll ->
                        submitManualUnlock(device, partition, buffer, enroll)
                    },
                    onDismissUnlock = {
                        promptDialogTarget = null
                        dialogNotice = null
                    },
                )
            }
        }
    }

    // Forget-biometric confirmation
    forgetTarget?.let { (bioKey, partition) ->
        ForgetBiometricDialog(
            label = partition.name.ifBlank { "Partition #${partition.index}" },
            onConfirm = {
                BiometricPassphraseVault.forget(context, bioKey)
                vaultVersion++
                forgetTarget = null
            },
            onDismiss = { forgetTarget = null },
        )
    }
}

@Composable
private fun QuietRoomHeader(
    isScanning: Boolean,
    onScan: () -> Unit,
    onOpenDiagnostics: (() -> Unit)?,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier = modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                imageVector = Icons.Default.Lock,
                contentDescription = null,
                tint = StateLocked,
                modifier = Modifier.size(20.dp),
            )
            Spacer(modifier = Modifier.width(8.dp))
            Text(
                text = "LUKS Android",
                style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.SemiBold),
                color = MaterialTheme.colorScheme.onBackground,
            )
        }

        Row(verticalAlignment = Alignment.CenterVertically) {
            if (isScanning) {
                CircularProgressIndicator(
                    modifier = Modifier.size(16.dp),
                    strokeWidth = 2.dp,
                    color = StateLocked,
                )
                Spacer(modifier = Modifier.width(8.dp))
            } else {
                IconButton(onClick = onScan) {
                    Icon(
                        imageVector = Icons.Default.Refresh,
                        contentDescription = "Check connection",
                        modifier = Modifier.size(18.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (onOpenDiagnostics != null) {
                IconButton(onClick = onOpenDiagnostics) {
                    Icon(
                        imageVector = Icons.Default.BugReport,
                        contentDescription = "Diagnostics",
                        modifier = Modifier.size(18.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

@Composable
private fun GlobalSessionNotices(
    sessionState: SessionState,
    deviceStates: Map<String, DeviceItemState>,
    onRetryPartition: (LuksDevice, PartitionInfo) -> Unit,
    onReset: () -> Unit,
    modifier: Modifier = Modifier,
) {
    when (val s = sessionState) {
        is SessionState.Unlocking -> {
            UnlockingStateCard(partition = s.partition, modifier = modifier)
        }
        is SessionState.Detached -> {
            DetachedStateCard(
                message = s.message,
                onDismiss = onReset,
                modifier = modifier,
            )
        }
        is SessionState.Failed -> {
            val part = s.partition
            val targetDev = if (part != null) {
                deviceStates.values
                    .filterIsInstance<DeviceItemState.Opened>()
                    .map { it.device }
                    .firstOrNull { it.openablePartitions.any { op -> op.offsetBytes == part.offsetBytes } }
            } else null

            FailedStateCard(
                message = s.message,
                onReset = onReset,
                onTryAgain = if (part != null && targetDev != null) {
                    { onRetryPartition(targetDev, part) }
                } else null,
                modifier = modifier,
            )
        }
        is SessionState.Unlocked -> {}
        is SessionState.Locked -> {}
    }
}

@Composable
private fun EmptyDevicesView(
    onScan: () -> Unit,
    isScanning: Boolean,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 24.dp),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Box(
                modifier = Modifier
                    .size(76.dp)
                    .clip(CircleShape)
                    .background(MaterialTheme.colorScheme.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    imageVector = Icons.Default.Usb,
                    contentDescription = null,
                    modifier = Modifier.size(36.dp),
                    tint = StateLocked,
                )
            }

            Text(
                text = "Plug in a drive",
                style = MaterialTheme.typography.titleLarge.copy(fontWeight = FontWeight.SemiBold),
                textAlign = TextAlign.Center,
                color = MaterialTheme.colorScheme.onSurface,
            )

            Text(
                text = "Nothing leaves this phone. Connect via USB-C or OTG adapter.\nDecryption keys remain strictly in RAM.",
                style = MaterialTheme.typography.bodyMedium,
                textAlign = TextAlign.Center,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(modifier = Modifier.height(4.dp))

            if (isScanning) {
                CircularProgressIndicator(
                    modifier = Modifier.size(20.dp),
                    strokeWidth = 2.dp,
                    color = StateLocked,
                )
            } else {
                TextButton(onClick = onScan) {
                    Icon(
                        imageVector = Icons.Default.Refresh,
                        contentDescription = null,
                        modifier = Modifier.size(16.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(modifier = Modifier.width(6.dp))
                    Text("Check USB connection", color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }
}

@Composable
private fun QuietSecurityFooter(modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(vertical = 8.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = Icons.Default.Security,
            contentDescription = null,
            modifier = Modifier.size(13.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.5f),
        )
        Spacer(modifier = Modifier.width(6.dp))
        Text(
            text = "Isolated RAM · Zero network · Off-heap key wiping",
            style = MaterialTheme.typography.labelSmall.copy(
                fontSize = 11.sp,
                letterSpacing = 0.3.sp,
            ),
            color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f),
        )
    }
}

@Composable
private fun TargetDeviceCard(
    target: UsbMassStorage.Target,
    hasPermission: Boolean,
    itemState: DeviceItemState,
    sessionState: SessionState,
    biometricAvailable: Boolean,
    biometricBusy: Boolean,
    vaultVersion: Int,
    promptDialogTarget: Pair<LuksDevice, PartitionInfo>?,
    dialogNotice: String?,
    onRequestPermission: () -> Unit,
    onOpen: () -> Unit,
    onUnlockPartition: (LuksDevice, PartitionInfo) -> Unit,
    onBiometricUnlockPartition: (LuksDevice, PartitionInfo) -> Unit,
    onMountPartition: (LuksDevice, PartitionInfo) -> Unit = { _, _ -> },
    onForgetBiometric: (String, PartitionInfo) -> Unit = { _, _ -> },
    onSubmitUnlock: (LuksDevice, PartitionInfo, SecurePassphraseBuffer, Boolean) -> Unit = { _, _, _, _ -> },
    onDismissUnlock: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    Surface(
        modifier = modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(16.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            // Header Row: USB Icon, Label
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    modifier = Modifier
                        .size(40.dp)
                        .clip(CircleShape)
                        .background(MaterialTheme.colorScheme.surface),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Default.Usb,
                        contentDescription = "USB",
                        tint = StateLocked,
                        modifier = Modifier.size(24.dp),
                    )
                }

                Spacer(modifier = Modifier.width(12.dp))

                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = target.label,
                        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.SemiBold),
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                    Text(
                        text = "USB Storage · Interface ${target.usbInterface.id}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.3f))

            // State specific content
            if (!hasPermission) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text(
                        text = "Tap to allow access to this drive",
                        style = MaterialTheme.typography.bodySmall,
                        color = WarningAmber,
                        modifier = Modifier.weight(1f),
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    Button(
                        onClick = onRequestPermission,
                        shape = RoundedCornerShape(8.dp),
                    ) {
                        Icon(
                            imageVector = Icons.Default.Key,
                            contentDescription = null,
                            modifier = Modifier.size(16.dp),
                        )
                        Spacer(modifier = Modifier.width(4.dp))
                        Text("Grant Permission")
                    }
                }
            } else {
                when (itemState) {
                    is DeviceItemState.Idle -> {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.SpaceBetween,
                        ) {
                            Text(
                                text = "Permission granted · Ready to inspect",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            Button(
                                onClick = onOpen,
                                shape = RoundedCornerShape(8.dp),
                            ) {
                                Text("Inspect Drive")
                            }
                        }
                    }

                    is DeviceItemState.Opening -> {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            CircularProgressIndicator(
                                modifier = Modifier.size(20.dp),
                                strokeWidth = 2.dp,
                                color = MaterialTheme.colorScheme.primary,
                            )
                            Text(
                                text = "Reading partition table…",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }

                    is DeviceItemState.Opened -> {
                        val dev = itemState.device
                        val info = dev.info

                        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                            Text(
                                text = "${info.vendor} ${info.product} · ${formatSize(info.sizeBytes)} · ${info.tableKind}",
                                style = MaterialTheme.typography.bodySmall.copy(fontWeight = FontWeight.Medium),
                                color = MaterialTheme.colorScheme.onSurface,
                            )

                            if (info.partitions.isNotEmpty()) {
                                DiskVisualMap(
                                    deviceInfo = info,
                                    selectedPartition = promptDialogTarget?.takeIf { it.first == dev }?.second,
                                    onSelectPartition = { partition ->
                                        onUnlockPartition(dev, partition)
                                    },
                                )
                            }

                            if (info.partitions.isEmpty()) {
                                Text(
                                    text = "No partition table found on drive.",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }

                            // Render partitions
                            info.partitions.forEach { partition ->
                                val bioKey = remember(dev, partition) { dev.biometricKeyFor(partition) }
                                val enrolled = remember(bioKey, vaultVersion) {
                                    bioKey != null && BiometricPassphraseVault.isEnrolled(context, bioKey)
                                }
                                val isPrompting = promptDialogTarget?.first == dev &&
                                    promptDialogTarget.second.offsetBytes == partition.offsetBytes
                                val offerEnroll = remember(bioKey, biometricAvailable, vaultVersion) {
                                    bioKey != null && biometricAvailable && !BiometricPassphraseVault.isEnrolled(context, bioKey)
                                }

                                PartitionItemCard(
                                    device = dev,
                                    partition = partition,
                                    sessionState = sessionState,
                                    biometricEnrolled = enrolled,
                                    showBiometricUnlock = enrolled && biometricAvailable,
                                    biometricBusy = biometricBusy,
                                    isPromptingUnlock = isPrompting,
                                    offerBiometricEnroll = offerEnroll,
                                    dialogNotice = if (isPrompting) dialogNotice else null,
                                    onUnlock = { onUnlockPartition(dev, partition) },
                                    onBiometricUnlock = { onBiometricUnlockPartition(dev, partition) },
                                    onMount = { onMountPartition(dev, partition) },
                                    onForgetBiometric = { bioKey?.let { onForgetBiometric(it, partition) } },
                                    onSubmitPassphrase = { buffer, enroll ->
                                        onSubmitUnlock(dev, partition, buffer, enroll)
                                    },
                                    onDismissUnlock = onDismissUnlock,
                                )
                            }
                        }
                    }

                    is DeviceItemState.Failed -> {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.SpaceBetween,
                        ) {
                            Text(
                                text = "Failed: ${itemState.error}",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.error,
                                modifier = Modifier.weight(1f),
                            )
                            OutlinedButton(onClick = onOpen, shape = RoundedCornerShape(8.dp)) {
                                Text("Retry")
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun PartitionItemCard(
    device: LuksDevice,
    partition: PartitionInfo,
    sessionState: SessionState,
    biometricEnrolled: Boolean,
    showBiometricUnlock: Boolean,
    biometricBusy: Boolean,
    isPromptingUnlock: Boolean,
    offerBiometricEnroll: Boolean,
    dialogNotice: String?,
    onUnlock: () -> Unit,
    onBiometricUnlock: () -> Unit = {},
    onMount: () -> Unit = {},
    onForgetBiometric: () -> Unit = {},
    onSubmitPassphrase: (SecurePassphraseBuffer, Boolean) -> Unit = { _, _ -> },
    onDismissUnlock: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val isSessionUnlocked = sessionState is SessionState.Unlocked &&
            sessionState.partition.offsetBytes == partition.offsetBytes

    Surface(
        modifier = modifier.fillMaxWidth(),
        shape = RoundedCornerShape(12.dp),
        color = MaterialTheme.colorScheme.surface,
        border = if (partition.isLuks) {
            BorderStroke(
                width = if (isPromptingUnlock) 1.5.dp else 1.dp,
                color = if (isSessionUnlocked) StateUnlocked.copy(alpha = 0.5f)
                        else if (isPromptingUnlock) StateLocked
                        else StateLocked.copy(alpha = 0.35f),
            )
        } else {
            BorderStroke(1.dp, DarkOutline)
        },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.weight(1f),
                ) {
                    Icon(
                        imageVector = if (partition.isLuks) {
                            if (isSessionUnlocked) Icons.Default.LockOpen else Icons.Default.Lock
                        } else {
                            Icons.Default.Storage
                        },
                        contentDescription = null,
                        tint = when {
                            isSessionUnlocked -> StateUnlocked
                            partition.isLuks -> StateLocked
                            partition.isPlain -> MaterialTheme.colorScheme.secondary
                            partition.unsupportedReason != null -> MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.5f)
                            else -> MaterialTheme.colorScheme.onSurfaceVariant
                        },
                        modifier = Modifier.size(20.dp),
                    )
                    Spacer(modifier = Modifier.width(10.dp))

                    Column {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                text = partition.name.ifBlank { "Partition #${partition.index}" },
                                style = MaterialTheme.typography.bodyMedium.copy(fontWeight = FontWeight.Medium),
                                color = MaterialTheme.colorScheme.onSurface,
                            )
                            if (partition.isLuks) {
                                Spacer(modifier = Modifier.width(6.dp))
                                Box(
                                    modifier = Modifier
                                        .clip(RoundedCornerShape(4.dp))
                                        .background(MaterialTheme.colorScheme.primaryContainer)
                                        .padding(horizontal = 6.dp, vertical = 1.dp),
                                ) {
                                    Text(
                                        text = "LUKS${partition.luksVersion ?: ""}",
                                        style = MaterialTheme.typography.labelSmall.copy(
                                            fontWeight = FontWeight.Bold,
                                            fontSize = 10.sp,
                                        ),
                                        color = MaterialTheme.colorScheme.onPrimaryContainer,
                                    )
                                }
                            } else if (partition.isPlain) {
                                Spacer(modifier = Modifier.width(6.dp))
                                Box(
                                    modifier = Modifier
                                        .clip(RoundedCornerShape(4.dp))
                                        .background(MaterialTheme.colorScheme.secondaryContainer)
                                        .padding(horizontal = 6.dp, vertical = 1.dp),
                                ) {
                                    Text(
                                        text = "${partition.fsType?.uppercase()}",
                                        style = MaterialTheme.typography.labelSmall.copy(
                                            fontWeight = FontWeight.Bold,
                                            fontSize = 10.sp,
                                        ),
                                        color = MaterialTheme.colorScheme.onSecondaryContainer,
                                    )
                                }
                            } else if (partition.unsupportedReason != null) {
                                Spacer(modifier = Modifier.width(6.dp))
                                Box(
                                    modifier = Modifier
                                        .clip(RoundedCornerShape(4.dp))
                                        .background(MaterialTheme.colorScheme.surfaceVariant)
                                        .padding(horizontal = 6.dp, vertical = 1.dp),
                                ) {
                                    Text(
                                        text = "${partition.fsType?.uppercase() ?: "UNKNOWN"} (Unsupported)",
                                        style = MaterialTheme.typography.labelSmall.copy(
                                            fontWeight = FontWeight.Medium,
                                            fontSize = 10.sp,
                                        ),
                                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    )
                                }
                            }

                            if (biometricEnrolled) {
                                Spacer(modifier = Modifier.width(6.dp))
                                BiometricRevokeIcon(
                                    onTriggerTurnOff = onForgetBiometric,
                                )
                            }
                        }

                        Text(
                            text = buildString {
                                append("${formatSize(partition.sizeBytes)} · Offset 0x${partition.offsetBytes.toString(16)}")
                                if (partition.unsupportedReason != null) {
                                    append(" · ${partition.unsupportedReason}")
                                }
                            },
                            style = MaterialTheme.typography.bodySmall.copy(
                                fontFamily = FontFamily.Monospace,
                                fontSize = 11.sp,
                            ),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }

                if (partition.isLuks) {
                    if (isSessionUnlocked) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Icon(
                                imageVector = Icons.Default.LockOpen,
                                contentDescription = "Unlocked",
                                tint = StateUnlocked,
                                modifier = Modifier.size(16.dp),
                            )
                            Spacer(modifier = Modifier.width(4.dp))
                            Text(
                                text = "Unlocked",
                                style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.Bold),
                                color = StateUnlocked,
                            )
                        }
                    } else if (sessionState is SessionState.Locked) {
                        AnimatedVisibility(
                            visible = !isPromptingUnlock,
                            enter = fadeIn(),
                            exit = fadeOut(),
                        ) {
                            FilledTonalButton(
                                onClick = if (showBiometricUnlock) onBiometricUnlock else onUnlock,
                                enabled = !biometricBusy,
                                shape = RoundedCornerShape(8.dp),
                            ) {
                                Icon(
                                    imageVector = if (showBiometricUnlock) Icons.Default.Fingerprint else Icons.Default.LockOpen,
                                    contentDescription = null,
                                    modifier = Modifier.size(16.dp),
                                )
                                Spacer(modifier = Modifier.width(4.dp))
                                Text("Unlock")
                            }
                        }
                    }
                } else if (partition.isPlain) {
                    if (isSessionUnlocked) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Icon(
                                imageVector = Icons.Default.CheckCircle,
                                contentDescription = "Mounted",
                                tint = StateUnlocked,
                                modifier = Modifier.size(16.dp),
                            )
                            Spacer(modifier = Modifier.width(4.dp))
                            Text(
                                text = "Mounted",
                                style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.Bold),
                                color = StateUnlocked,
                            )
                        }
                    } else if (sessionState is SessionState.Locked) {
                        FilledTonalButton(
                            onClick = onMount,
                            shape = RoundedCornerShape(8.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Default.Storage,
                                contentDescription = null,
                                modifier = Modifier.size(16.dp),
                            )
                            Spacer(modifier = Modifier.width(4.dp))
                            Text("Mount")
                        }
                    }
                }
            }

            // In-place Unlock Area
            AnimatedVisibility(
                visible = isPromptingUnlock && partition.isLuks && sessionState is SessionState.Locked,
                enter = expandVertically(expandFrom = Alignment.Top) + fadeIn(),
                exit = shrinkVertically(shrinkTowards = Alignment.Top) + fadeOut(),
            ) {
                InPlaceUnlockSection(
                    partition = partition,
                    showBiometricUnlock = showBiometricUnlock,
                    biometricBusy = biometricBusy,
                    offerBiometricEnroll = offerBiometricEnroll,
                    dialogNotice = dialogNotice,
                    onBiometricUnlock = onBiometricUnlock,
                    onSubmitPassphrase = onSubmitPassphrase,
                    onDismiss = onDismissUnlock,
                )
            }
        }
    }
}

@Composable
private fun InPlaceUnlockSection(
    partition: PartitionInfo,
    showBiometricUnlock: Boolean,
    biometricBusy: Boolean,
    offerBiometricEnroll: Boolean,
    dialogNotice: String?,
    onBiometricUnlock: () -> Unit,
    onSubmitPassphrase: (SecurePassphraseBuffer, Boolean) -> Unit,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    var activeEditable by remember { mutableStateOf<Editable?>(null) }
    var hasContent by remember { mutableStateOf(false) }
    var enrollChecked by remember { mutableStateOf(false) }

    fun submit() {
        val editable = activeEditable ?: return
        val buffer = PassphraseScrubber.extractAndScrub(editable)
        onSubmitPassphrase(buffer, offerBiometricEnroll && enrollChecked)
    }

    Column(
        modifier = modifier
            .fillMaxWidth()
            .padding(top = 8.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.25f))

        if (dialogNotice != null) {
            Text(
                text = dialogNotice,
                style = MaterialTheme.typography.bodySmall,
                color = WarningAmber,
            )
        }

        if (showBiometricUnlock) {
            FilledTonalButton(
                onClick = onBiometricUnlock,
                enabled = !biometricBusy,
                shape = RoundedCornerShape(8.dp),
                modifier = Modifier
                    .fillMaxWidth()
                    .height(36.dp),
                colors = ButtonDefaults.filledTonalButtonColors(
                    containerColor = StateUnlocked.copy(alpha = 0.12f),
                    contentColor = StateUnlocked,
                ),
                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 0.dp),
            ) {
                Icon(
                    imageVector = Icons.Default.Fingerprint,
                    contentDescription = null,
                    modifier = Modifier.size(16.dp),
                )
                Spacer(modifier = Modifier.width(6.dp))
                Text(
                    text = "Touch sensor to unlock with Fingerprint",
                    style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.Medium),
                )
            }
        }

        // Compact Passphrase Entry Row: Input + Unlock button side-by-side
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Box(modifier = Modifier.weight(1f)) {
                SecurePassphraseField(
                    onEditableReady = { activeEditable = it },
                    onHasContentChange = { hasContent = it },
                )
            }

            FilledTonalButton(
                onClick = ::submit,
                enabled = hasContent,
                shape = RoundedCornerShape(8.dp),
                modifier = Modifier.height(40.dp),
                contentPadding = PaddingValues(horizontal = 14.dp, vertical = 0.dp),
            ) {
                Icon(
                    imageVector = Icons.Default.LockOpen,
                    contentDescription = null,
                    modifier = Modifier.size(15.dp),
                )
                Spacer(modifier = Modifier.width(4.dp))
                Text(
                    text = "Unlock",
                    style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.SemiBold),
                )
            }
        }

        // Sub-row: Memory security reassurance + Cancel affordance
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "Protected memory · Wiped on lock",
                style = MaterialTheme.typography.labelSmall.copy(
                    fontSize = 10.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.7f),
                ),
            )

            TextButton(
                onClick = onDismiss,
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 0.dp),
                modifier = Modifier.height(26.dp),
            ) {
                Text(
                    text = "Cancel",
                    style = MaterialTheme.typography.labelSmall.copy(color = MaterialTheme.colorScheme.onSurfaceVariant),
                )
            }
        }

        if (offerBiometricEnroll) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .fillMaxWidth()
                    .toggleable(
                        value = enrollChecked,
                        role = Role.Checkbox,
                        onValueChange = { enrollChecked = it },
                    )
                    .padding(vertical = 2.dp),
            ) {
                Checkbox(
                    checked = enrollChecked,
                    onCheckedChange = null,
                    modifier = Modifier.size(18.dp),
                )
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    text = "Save passphrase to fingerprint for this drive",
                    style = MaterialTheme.typography.bodySmall.copy(fontSize = 11.sp),
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }
        }
    }
}

@Composable
private fun UnlockingStateCard(
    partition: PartitionInfo,
    modifier: Modifier = Modifier,
) {
    var elapsedSec by remember { mutableIntStateOf(0) }

    LaunchedEffect(partition) {
        val start = System.currentTimeMillis()
        while (true) {
            elapsedSec = ((System.currentTimeMillis() - start) / 1000).toInt()
            delay(500)
        }
    }

    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.primaryContainer,
        ),
        shape = RoundedCornerShape(12.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                CircularProgressIndicator(
                    modifier = Modifier.size(24.dp),
                    color = MaterialTheme.colorScheme.onPrimaryContainer,
                    strokeWidth = 2.5.dp,
                )
                Column {
                    Text(
                        text = "Unlocking ${partition.name.ifBlank { "Partition #${partition.index}" }}…",
                        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                        color = MaterialTheme.colorScheme.onPrimaryContainer,
                    )
                    Text(
                        text = "Deriving key — slow on purpose, so guessing is slow too.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.85f),
                    )
                }
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                val kdfLabel = when (partition.luksVersion) {
                    1 -> "PBKDF2 · Iterated KDF"
                    2 -> "Argon2id · Memory-hard KDF"
                    else -> "Cryptographic KDF"
                }
                Text(
                    text = kdfLabel,
                    style = MaterialTheme.typography.labelSmall.copy(fontFamily = FontFamily.Monospace),
                    color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.7f),
                )
                Text(
                    text = "${elapsedSec}s elapsed",
                    style = MaterialTheme.typography.labelSmall.copy(fontFamily = FontFamily.Monospace, fontWeight = FontWeight.Bold),
                    color = MaterialTheme.colorScheme.onPrimaryContainer,
                )
            }
        }
    }
}

@Composable
private fun DetachedStateCard(
    message: String,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant,
        ),
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.weight(1f),
            ) {
                Icon(
                    imageVector = Icons.Default.UsbOff,
                    contentDescription = null,
                    tint = StateLocked,
                    modifier = Modifier.size(28.dp),
                )
                Spacer(modifier = Modifier.width(12.dp))
                Column {
                    Text(
                        text = "Drive Disconnected",
                        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                    Text(
                        text = "Session closed safely. Master key cleared from memory.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            Button(
                onClick = onDismiss,
                shape = RoundedCornerShape(8.dp),
            ) {
                Text("Scan Drives")
            }
        }
    }
}

@Composable
private fun FailedStateCard(
    message: String,
    onReset: () -> Unit,
    onTryAgain: (() -> Unit)?,
    modifier: Modifier = Modifier,
) {
    val isWrongPassphrase = message.contains("wrong passphrase", ignoreCase = true)
    val autoDismissProgress = remember { Animatable(0f) }

    LaunchedEffect(Unit) {
        autoDismissProgress.animateTo(
            targetValue = 1f,
            animationSpec = tween(durationMillis = 4000, easing = LinearEasing),
        )
        onReset()
    }

    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant,
        ),
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, if (isWrongPassphrase) StateUnlocked.copy(alpha = 0.5f) else DestructiveRed.copy(alpha = 0.5f)),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    imageVector = if (isWrongPassphrase) Icons.Default.Lock else Icons.Default.ErrorOutline,
                    contentDescription = null,
                    tint = if (isWrongPassphrase) StateUnlocked else DestructiveRed,
                    modifier = Modifier.size(24.dp),
                )
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    text = if (isWrongPassphrase) "Incorrect Passphrase" else "Unlock Failed",
                    style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }

            if (!isWrongPassphrase) {
                Text(
                    text = message,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (onTryAgain != null) {
                    Button(
                        onClick = onTryAgain,
                        shape = RoundedCornerShape(8.dp),
                        modifier = Modifier.height(38.dp),
                        contentPadding = PaddingValues(horizontal = 14.dp, vertical = 0.dp),
                    ) {
                        Text("Try Again")
                    }
                }

                // Auto-dismiss bar: fills from 0% to 100% over 4s (tap to dismiss early)
                val fillTrackColor = if (isWrongPassphrase) StateUnlocked.copy(alpha = 0.22f)
                                     else DestructiveRed.copy(alpha = 0.20f)
                Box(
                    modifier = (if (onTryAgain != null) Modifier.weight(1f) else Modifier.fillMaxWidth())
                        .height(38.dp)
                        .clip(RoundedCornerShape(8.dp))
                        .background(MaterialTheme.colorScheme.surface)
                        .border(
                            BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.4f)),
                            RoundedCornerShape(8.dp),
                        )
                        .drawBehind {
                            val fraction = autoDismissProgress.value.coerceIn(0f, 1f)
                            if (fraction > 0f) {
                                drawRect(
                                    color = fillTrackColor,
                                    size = size.copy(width = size.width * fraction),
                                )
                            }
                        }
                        .clickable(onClick = onReset),
                    contentAlignment = Alignment.Center,
                ) {
                    Row(
                        modifier = Modifier.fillMaxSize(),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Default.Close,
                            contentDescription = null,
                            modifier = Modifier.size(14.dp),
                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        Spacer(modifier = Modifier.width(6.dp))
                        Text(
                            text = "Dismiss",
                            style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.Medium),
                            color = MaterialTheme.colorScheme.onSurface,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun UnlockedStateCard(
    state: SessionState.Unlocked,
    onNavigateToBrowser: (() -> Unit)?,
    onLock: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val info = state.volume.info

    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant,
        ),
        shape = RoundedCornerShape(12.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            if (!info.encrypted) {
                Card(
                    modifier = Modifier.fillMaxWidth(),
                    colors = CardDefaults.cardColors(
                        containerColor = MaterialTheme.colorScheme.errorContainer,
                    ),
                    shape = RoundedCornerShape(8.dp),
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            text = "⚠️ UNENCRYPTED DRIVE: Data on this partition is stored in plaintext.",
                            style = MaterialTheme.typography.labelMedium.copy(
                                fontWeight = FontWeight.Bold,
                            ),
                            color = MaterialTheme.colorScheme.onErrorContainer,
                        )
                    }
                }
            }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Icon(
                    imageVector = if (info.encrypted) Icons.Default.LockOpen else Icons.Default.Storage,
                    contentDescription = null,
                    tint = StateUnlocked,
                    modifier = Modifier.size(24.dp),
                )
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = "Active Session: ${info.label.ifBlank { state.partition.label }}",
                        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                    Text(
                        text = "Filesystem: ${info.fsType.uppercase()} · ${formatSize(info.sizeBytes)}" +
                            if (!info.encrypted) " · Unencrypted" else "",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (onNavigateToBrowser != null) {
                    Button(
                        onClick = onNavigateToBrowser,
                        modifier = Modifier.weight(1f),
                        shape = RoundedCornerShape(8.dp),
                    ) {
                        Icon(imageVector = Icons.Default.FolderOpen, contentDescription = null, modifier = Modifier.size(16.dp))
                        Spacer(modifier = Modifier.width(6.dp))
                        Text("Open File Browser")
                    }
                }

                OutlinedButton(
                    onClick = {
                        LuksHaptics.deadboltLock(context)
                        onLock()
                    },
                    shape = RoundedCornerShape(8.dp),
                ) {
                    Icon(
                        imageVector = if (info.encrypted) Icons.Default.Lock else Icons.Default.LockOpen,
                        contentDescription = null,
                        modifier = Modifier.size(16.dp),
                    )
                    Spacer(modifier = Modifier.width(4.dp))
                    Text(if (info.encrypted) "Lock" else "Unmount")
                }
            }
        }
    }
}


@Composable
private fun BiometricRevokeIcon(
    onTriggerTurnOff: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val holdProgress = remember { Animatable(0f) }
    var isHolding by remember { mutableStateOf(false) }

    // Guarantee vibrator motor is cancelled if composable unmounts while being touched
    DisposableEffect(Unit) {
        onDispose {
            LuksHaptics.cancel(context)
        }
    }

    // Faded dark reddish tones (calm, subtle, non-intrusive)
    val containerBg = Color(0xFF281A1C)
    val iconTint = Color(0xFFC76D6D)
    val activeBorder = DestructiveRed.copy(alpha = 0.7f)

    Box(
        modifier = modifier
            .size(24.dp)
            .clip(RoundedCornerShape(6.dp))
            .background(containerBg)
            .border(
                BorderStroke(
                    width = 1.dp,
                    color = if (isHolding) activeBorder else iconTint.copy(alpha = 0.25f),
                ),
                shape = RoundedCornerShape(6.dp),
            )
            .drawBehind {
                val fraction = holdProgress.value
                if (fraction > 0f) {
                    drawRect(activeBorder.copy(alpha = 0.35f * fraction))
                }
            }
            .pointerInput(Unit) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    isHolding = true
                    var triggered = false

                    val holdJob = scope.launch {
                        LuksHaptics.startUphillRamp(context)
                        holdProgress.snapTo(0f)
                        holdProgress.animateTo(
                            targetValue = 1f,
                            animationSpec = tween(durationMillis = 650, easing = LinearEasing),
                        )
                        triggered = true
                        LuksHaptics.rampPeakAndDrop(context)
                        onTriggerTurnOff()
                    }

                    try {
                        waitForUpOrCancellation()
                    } finally {
                        if (!triggered) {
                            holdJob.cancel()
                            LuksHaptics.cancel(context)
                            scope.launch {
                                holdProgress.animateTo(0f, animationSpec = tween(120))
                            }
                        }
                        isHolding = false
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            imageVector = Icons.Default.Fingerprint,
            contentDescription = "Fingerprint enrolled. Touch and hold to turn off.",
            tint = iconTint,
            modifier = Modifier.size(13.dp),
        )
    }
}

@Composable
private fun ForgetBiometricDialog(
    label: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = {
            Text(
                text = "Turn off fingerprint unlock?",
                style = MaterialTheme.typography.titleLarge,
                color = MaterialTheme.colorScheme.error,
            )
        },
        text = {
            Column(
                modifier = Modifier.fillMaxWidth(),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text(
                    text = "Are you sure you want to turn off fingerprint unlock for \"$label\"?",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Text(
                    text = "The saved passphrase will be removed from secure hardware storage.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                    fontWeight = FontWeight.Medium,
                )
                Text(
                    text = "You will need to manually type the passphrase next time you unlock this partition.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = {
            Button(
                onClick = {
                    LuksHaptics.deadboltLock(context)
                    onConfirm()
                },
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.error,
                    contentColor = MaterialTheme.colorScheme.onError,
                ),
                shape = RoundedCornerShape(8.dp),
            ) {
                Text("Turn Off")
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancel")
            }
        },
        shape = RoundedCornerShape(16.dp),
    )
}

private suspend fun openTarget(
    context: Context,
    target: UsbMassStorage.Target,
): DeviceItemState {
    var device: LuksDevice? = null
    return try {
        device = withContext(Dispatchers.IO) {
            UsbMassStorage.open(context, target)
        }
        Trace.i("DevicesScreen: opened ${target.label} successfully")
        DeviceItemState.Opened(device)
    } catch (e: CancellationException) {
        runCatching { device?.close() }
        throw e
    } catch (e: LuksException) {
        runCatching { device?.close() }
        Trace.err(e.code, "open_target")
        Trace.e("DevicesScreen: open failed [${e.code}]")
        DeviceItemState.Failed("[${e.code}] ${e.message}")
    } catch (e: Exception) {
        if (e is CancellationException) {
            runCatching { device?.close() }
            throw e
        }
        runCatching { device?.close() }
        Trace.err(-1, "open_target")
        Trace.e("DevicesScreen: open failed: ${Trace.throwableSummary(e)}")
        DeviceItemState.Failed(e.message ?: e.toString())
    }
}
