package dev.luksandroid.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Storage
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.luksandroid.DeviceInfo
import dev.luksandroid.PartitionInfo
import dev.luksandroid.formatSize
import dev.luksandroid.ui.LuksHaptics
import dev.luksandroid.ui.theme.DarkOutline
import dev.luksandroid.ui.theme.DarkOutlineVariant
import dev.luksandroid.ui.theme.StateLocked
import dev.luksandroid.ui.theme.StateUnlocked
import dev.luksandroid.ui.theme.TextMuted
import dev.luksandroid.ui.theme.TextPrimary

private sealed class DiskSegment {
    data class Partition(val partition: PartitionInfo, val weight: Float) : DiskSegment()
    data class Unallocated(val sizeBytes: Long, val weight: Float) : DiskSegment()
}

/**
 * Visual proportional disk map that renders the physical layout of the drive.
 * Displays all partitions (LUKS, plain filesystems, unsupported/ESP) in spatial proportion,
 * clearly presenting parallel layout, partition boundaries, and size distribution.
 */
@Composable
fun DiskVisualMap(
    deviceInfo: DeviceInfo,
    selectedPartition: PartitionInfo?,
    onSelectPartition: (PartitionInfo) -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val totalBytes = deviceInfo.sizeBytes
    val partitions = deviceInfo.partitions

    if (totalBytes <= 0 || partitions.isEmpty()) {
        return
    }

    // Build segments with ergonomic minimum display weights so tiny partitions
    // (such as 16MB/512MB EFI or boot sectors) remain visible, touchable, and legible.
    val segments = remember(partitions, totalBytes) {
        val allocatedBytes = partitions.sumOf { it.sizeBytes }
        val unallocatedBytes = (totalBytes - allocatedBytes).coerceAtLeast(0L)

        val totalDisplayDenominator = totalBytes.toDouble().coerceAtLeast(1.0)
        val result = mutableListOf<DiskSegment>()

        partitions.forEach { p ->
            val rawFraction = (p.sizeBytes.toDouble() / totalDisplayDenominator).toFloat()
            // Clamp min display weight to 0.12f so small slices stay interactive
            val displayWeight = rawFraction.coerceAtLeast(0.12f)
            result.add(DiskSegment.Partition(p, displayWeight))
        }

        // Show unallocated segment if there is significant slack (> 32MB)
        if (unallocatedBytes > 32L * 1024 * 1024) {
            val rawFraction = (unallocatedBytes.toDouble() / totalDisplayDenominator).toFloat()
            val displayWeight = rawFraction.coerceAtLeast(0.08f)
            result.add(DiskSegment.Unallocated(unallocatedBytes, displayWeight))
        }

        result
    }

    Column(
        modifier = modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        // Physical Partition Ribbon
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(34.dp)
                .clip(RoundedCornerShape(8.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.5f))
                .border(1.dp, DarkOutline, RoundedCornerShape(8.dp))
                .padding(2.dp),
            horizontalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            segments.forEach { segment ->
                when (segment) {
                    is DiskSegment.Partition -> {
                        val partition = segment.partition
                        val isSelected = selectedPartition?.offsetBytes == partition.offsetBytes

                        val (bgColor, borderColor, contentColor) = when {
                            partition.isLuks -> Triple(
                                if (isSelected) StateLocked.copy(alpha = 0.35f) else StateLocked.copy(alpha = 0.18f),
                                if (isSelected) StateLocked else StateLocked.copy(alpha = 0.45f),
                                StateLocked,
                            )
                            partition.isPlain -> Triple(
                                if (isSelected) MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.8f)
                                else MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.45f),
                                if (isSelected) StateUnlocked else DarkOutlineVariant,
                                StateUnlocked,
                            )
                            else -> Triple(
                                if (isSelected) MaterialTheme.colorScheme.surfaceVariant
                                else MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.6f),
                                if (isSelected) MaterialTheme.colorScheme.outline else DarkOutline,
                                TextMuted,
                            )
                        }

                        Box(
                            modifier = Modifier
                                .weight(segment.weight)
                                .height(30.dp)
                                .clip(RoundedCornerShape(6.dp))
                                .background(bgColor)
                                .border(
                                    width = if (isSelected) 1.5.dp else 1.dp,
                                    color = borderColor,
                                    shape = RoundedCornerShape(6.dp),
                                )
                                .clickable {
                                    LuksHaptics.tick(context)
                                    onSelectPartition(partition)
                                }
                                .padding(horizontal = 4.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.Center,
                            ) {
                                if (partition.isLuks) {
                                    Icon(
                                        imageVector = Icons.Default.Lock,
                                        contentDescription = null,
                                        tint = contentColor,
                                        modifier = Modifier.size(11.dp),
                                    )
                                    Spacer(modifier = Modifier.width(3.dp))
                                }

                                Text(
                                    text = buildString {
                                        if (partition.isLuks) {
                                            append("LUKS${partition.luksVersion ?: ""}")
                                        } else if (partition.isPlain) {
                                            append(partition.fsType?.uppercase() ?: "EXT4")
                                        } else {
                                            append(partition.fsType?.uppercase() ?: "#${partition.index}")
                                        }
                                        append(" · ${formatSize(partition.sizeBytes)}")
                                    },
                                    style = MaterialTheme.typography.labelSmall.copy(
                                        fontSize = 10.sp,
                                        fontWeight = if (isSelected) FontWeight.Bold else FontWeight.Medium,
                                        fontFamily = FontFamily.Monospace,
                                    ),
                                    color = if (isSelected) TextPrimary else contentColor,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                        }
                    }

                    is DiskSegment.Unallocated -> {
                        Box(
                            modifier = Modifier
                                .weight(segment.weight)
                                .height(30.dp)
                                .clip(RoundedCornerShape(6.dp))
                                .background(MaterialTheme.colorScheme.background.copy(alpha = 0.6f))
                                .border(1.dp, DarkOutline.copy(alpha = 0.5f), RoundedCornerShape(6.dp))
                                .padding(horizontal = 4.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                text = "Free ${formatSize(segment.sizeBytes)}",
                                style = MaterialTheme.typography.labelSmall.copy(
                                    fontSize = 9.sp,
                                    color = TextMuted.copy(alpha = 0.7f),
                                    fontFamily = FontFamily.Monospace,
                                ),
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                }
            }
        }

        // Sub-ribbon physical metrics
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 2.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "Sector 0 · Offset 0x0",
                style = MaterialTheme.typography.labelSmall.copy(
                    fontSize = 10.sp,
                    fontFamily = FontFamily.Monospace,
                    color = TextMuted.copy(alpha = 0.8f),
                ),
            )
            Text(
                text = "${formatSize(totalBytes)} Physical Disk · ${partitions.size} partitions",
                style = MaterialTheme.typography.labelSmall.copy(
                    fontSize = 10.sp,
                    fontFamily = FontFamily.Monospace,
                    color = TextMuted.copy(alpha = 0.8f),
                ),
            )
        }
    }
}
