package dev.luksandroid.ui.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.HourglassEmpty
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Storage
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.luksandroid.formatSize
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionState
import androidx.compose.foundation.border
import androidx.compose.material.icons.filled.LockOpen
import dev.luksandroid.ui.theme.DarkOutline
import dev.luksandroid.ui.theme.DestructiveRed
import dev.luksandroid.ui.theme.StateLocked
import dev.luksandroid.ui.theme.StateUnlocked
import dev.luksandroid.ui.theme.TextMuted
import dev.luksandroid.ui.theme.TextPrimary
import kotlinx.coroutines.launch

@Composable
fun SessionStatusHeader(
    modifier: Modifier = Modifier,
) {
    val sessionState by LuksSession.state.collectAsState()
    val activeLeases by LuksSession.activeLeasesFlow.collectAsState()
    val remainingIdleMs by LuksSession.remainingIdleMs.collectAsState()
    val scope = rememberCoroutineScope()

    AnimatedVisibility(
        visible = sessionState !is SessionState.Locked,
        enter = fadeIn(),
        exit = fadeOut(),
        modifier = modifier,
    ) {
        when (val state = sessionState) {
            is SessionState.Unlocked -> {
                val volInfo = state.volume.info
                val label = volInfo.label.ifBlank { state.partition.label }
                val fsType = volInfo.fsType.uppercase()
                val isTimerPaused = activeLeases > 0

                Surface(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp)
                        .border(
                            width = 1.dp,
                            color = StateUnlocked.copy(alpha = 0.35f),
                            shape = RoundedCornerShape(10.dp),
                        ),
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = RoundedCornerShape(10.dp),
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        // Left: Storage Icon, Label, FileSystem badge, and Live Timer/Lease chip
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier.weight(1f, fill = false),
                        ) {
                            Icon(
                                imageVector = Icons.Default.LockOpen,
                                contentDescription = "Unlocked",
                                tint = StateUnlocked,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(modifier = Modifier.width(6.dp))
                            Text(
                                text = label,
                                style = MaterialTheme.typography.titleSmall.copy(fontWeight = FontWeight.Bold),
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                color = TextPrimary,
                            )
                            Spacer(modifier = Modifier.width(6.dp))
                            // Filesystem pill badge
                            Box(
                                modifier = Modifier
                                    .clip(RoundedCornerShape(4.dp))
                                    .background(StateUnlocked.copy(alpha = 0.15f))
                                    .padding(horizontal = 5.dp, vertical = 1.dp),
                            ) {
                                Text(
                                    text = fsType,
                                    style = MaterialTheme.typography.labelSmall.copy(
                                        fontWeight = FontWeight.Bold,
                                        fontSize = 10.sp,
                                    ),
                                    color = StateUnlocked,
                                )
                            }
                            Spacer(modifier = Modifier.width(8.dp))
                            // Live lease / relock countdown pill
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                modifier = Modifier
                                    .clip(RoundedCornerShape(4.dp))
                                    .background(
                                        if (isTimerPaused) DestructiveRed.copy(alpha = 0.15f)
                                        else StateUnlocked.copy(alpha = 0.10f)
                                    )
                                    .padding(horizontal = 6.dp, vertical = 2.dp),
                            ) {
                                Icon(
                                    imageVector = if (isTimerPaused) Icons.Default.Sync else Icons.Default.HourglassEmpty,
                                    contentDescription = null,
                                    tint = if (isTimerPaused) DestructiveRed else StateUnlocked,
                                    modifier = Modifier.size(12.dp),
                                )
                                Spacer(modifier = Modifier.width(3.dp))
                                Text(
                                    text = if (isTimerPaused) {
                                        "Writing — don't unplug"
                                    } else {
                                        "Auto-locks in ${formatIdleTime(remainingIdleMs)}"
                                    },
                                    style = MaterialTheme.typography.labelSmall.copy(
                                        fontFamily = FontFamily.Monospace,
                                        fontSize = 11.sp,
                                        fontWeight = FontWeight.Medium,
                                    ),
                                    color = if (isTimerPaused) DestructiveRed else StateUnlocked,
                                )
                            }
                        }

                        Spacer(modifier = Modifier.width(8.dp))

                        // 1-tap Lock button (no confirmation needed - locking is immediate)
                        FilledTonalButton(
                            onClick = {
                                scope.launch { LuksSession.lock() }
                            },
                            colors = ButtonDefaults.filledTonalButtonColors(
                                containerColor = if (isTimerPaused) DestructiveRed.copy(alpha = 0.2f) else StateUnlocked.copy(alpha = 0.15f),
                                contentColor = if (isTimerPaused) DestructiveRed else StateUnlocked,
                            ),
                            contentPadding = PaddingValues(horizontal = 10.dp, vertical = 0.dp),
                            modifier = Modifier.height(32.dp),
                            shape = RoundedCornerShape(8.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Default.Lock,
                                contentDescription = null,
                                modifier = Modifier.size(14.dp),
                            )
                            Spacer(modifier = Modifier.width(4.dp))
                            Text(
                                text = if (isTimerPaused) "Lock after write" else "Lock",
                                style = MaterialTheme.typography.labelMedium.copy(fontWeight = FontWeight.SemiBold),
                            )
                        }
                    }
                }
            }

            is SessionState.Detached -> {
                Surface(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp)
                        .border(1.dp, DarkOutline, RoundedCornerShape(8.dp)),
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = RoundedCornerShape(8.dp),
                ) {
                    Row(
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Default.Lock,
                            contentDescription = "Detached",
                            tint = StateLocked,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Text(
                            text = "Drive disconnected. Session closed safely. Memory wiped.",
                            style = MaterialTheme.typography.bodySmall,
                            color = TextPrimary,
                        )
                    }
                }
            }

            is SessionState.Failed -> {
                Surface(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp)
                        .border(1.dp, DestructiveRed.copy(alpha = 0.3f), RoundedCornerShape(8.dp)),
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = RoundedCornerShape(8.dp),
                ) {
                    Row(
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Default.Warning,
                            contentDescription = "Failed",
                            tint = DestructiveRed,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Text(
                            text = "Session error: ${state.message}",
                            style = MaterialTheme.typography.bodySmall,
                            color = TextPrimary,
                        )
                    }
                }
            }

            is SessionState.Unlocking -> {
                Surface(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = RoundedCornerShape(8.dp),
                ) {
                    Row(
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Default.Sync,
                            contentDescription = "Unlocking",
                            tint = MaterialTheme.colorScheme.primary,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Text(
                            text = "Deriving key and unlocking ${state.partition.label}…",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }

            is SessionState.Locked -> {
                // Hidden by AnimatedVisibility
            }
        }
    }
}

private fun formatIdleTime(ms: Long): String {
    val totalSecs = (ms / 1000).coerceAtLeast(0)
    val mins = totalSecs / 60
    val secs = totalSecs % 60
    return "%d:%02d".format(mins, secs)
}
