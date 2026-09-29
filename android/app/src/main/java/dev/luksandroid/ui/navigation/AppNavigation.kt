package dev.luksandroid.ui.navigation

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import dev.luksandroid.BuildConfig
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionState
import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferManager
import dev.luksandroid.session.TransferState
import dev.luksandroid.ui.transfers.formatSpeed
import dev.luksandroid.ui.browser.BrowserScreen
import dev.luksandroid.ui.browser.viewer.MediaPlayerScreen
import dev.luksandroid.ui.components.SessionStatusHeader
import dev.luksandroid.ui.devices.DevicesScreen
import dev.luksandroid.ui.diagnostics.DiagnosticsScreen
import dev.luksandroid.ui.theme.StateUnlocked
import dev.luksandroid.ui.theme.TextPrimary
import dev.luksandroid.ui.transfers.TransfersScreen

data class ActiveMedia(val fullPath: String, val displayName: String)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LuksAppNavigation(
    modifier: Modifier = Modifier,
) {
    val sessionState by LuksSession.state.collectAsState()
    val isUnlocked = sessionState is SessionState.Unlocked
    var activeMedia by remember { mutableStateOf<ActiveMedia?>(null) }
    var showTransfersSheet by remember { mutableStateOf(false) }
    var showDiagnosticsSheet by remember { mutableStateOf(false) }

    val transfers by TransferManager.transfers.collectAsState()
    val activeTransfers = remember(transfers) {
        transfers.filter { it.state == TransferState.RUNNING || it.state == TransferState.QUEUED }
    }

    Box(
        modifier = modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            .statusBarsPadding()
            .navigationBarsPadding()
            .imePadding(),
    ) {
        Column(modifier = Modifier.fillMaxSize()) {
            // Door Open Guardian: Amber status bar with lease warnings, idle countdown, and 1-tap lock
            if (activeMedia == null && isUnlocked) {
                SessionStatusHeader()
            }

            // Unified Single-Door Canvas
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth(),
            ) {
                AnimatedContent(
                    targetState = isUnlocked,
                    transitionSpec = {
                        if (targetState) {
                            (fadeIn(animationSpec = tween(300)) + scaleIn(
                                initialScale = 0.98f,
                                animationSpec = spring(
                                    dampingRatio = Spring.DampingRatioNoBouncy,
                                    stiffness = Spring.StiffnessLow,
                                )
                            )).togetherWith(fadeOut(animationSpec = tween(200)))
                        } else {
                            fadeIn(animationSpec = tween(200))
                                .togetherWith(fadeOut(animationSpec = tween(200)) + scaleOut(targetScale = 0.98f, animationSpec = tween(200)))
                        }
                    },
                    label = "VaultStateTransition",
                ) { unlocked ->
                    if (unlocked) {
                        BrowserScreen(
                            onNavigateToDevices = {
                                // Locked - LuksSession will transition state
                            },
                            onPlayMedia = { fullPath, displayName ->
                                activeMedia = ActiveMedia(fullPath, displayName)
                            },
                        )
                    } else {
                        DevicesScreen(
                            onOpenDiagnostics = if (BuildConfig.DEBUG) {
                                { showDiagnosticsSheet = true }
                            } else null,
                        )
                    }
                }
            }
        }

        // Ambient Transfer Pill: Only surfaces when files are actively moving
        AnimatedVisibility(
            visible = activeMedia == null && activeTransfers.isNotEmpty(),
            enter = slideInVertically(initialOffsetY = { it }) + fadeIn(),
            exit = slideOutVertically(targetOffsetY = { it }) + fadeOut(),
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(bottom = 24.dp),
        ) {
            AmbientTransferPill(
                activeTransfers = activeTransfers,
                onClick = { showTransfersSheet = true },
            )
        }

        // Ambient Transfers Sheet
        if (showTransfersSheet) {
            ModalBottomSheet(
                onDismissRequest = { showTransfersSheet = false },
                containerColor = MaterialTheme.colorScheme.surface,
            ) {
                TransfersScreen(
                    onBack = { showTransfersSheet = false },
                )
            }
        }

        // Quiet Diagnostics Sheet (Debug only)
        if (showDiagnosticsSheet) {
            ModalBottomSheet(
                onDismissRequest = { showDiagnosticsSheet = false },
                containerColor = MaterialTheme.colorScheme.surface,
            ) {
                DiagnosticsScreen()
            }
        }

        // In-Memory Media Player Overlay
        activeMedia?.let { media ->
            MediaPlayerScreen(
                fullPath = media.fullPath,
                displayName = media.displayName,
                onClose = { activeMedia = null },
            )
        }
    }
}

@Composable
private fun AmbientTransferPill(
    activeTransfers: List<TransferItem>,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val count = activeTransfers.size
    val totalSpeed = activeTransfers.sumOf { it.speedBytesPerSec }

    Surface(
        modifier = modifier
            .clip(RoundedCornerShape(24.dp))
            .clickable(onClick = onClick),
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(24.dp),
        border = BorderStroke(1.dp, StateUnlocked.copy(alpha = 0.5f)),
        shadowElevation = 8.dp,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(16.dp),
                strokeWidth = 2.dp,
                color = StateUnlocked,
            )
            Text(
                text = if (count == 1) "1 transfer active" else "$count transfers active",
                style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.SemiBold),
                color = TextPrimary,
            )
            if (totalSpeed > 0) {
                Text(
                    text = "· ${formatSpeed(totalSpeed)}",
                    style = MaterialTheme.typography.labelMedium,
                    color = StateUnlocked,
                )
            }
        }
    }
}
