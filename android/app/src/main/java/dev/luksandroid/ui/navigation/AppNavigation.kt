package dev.luksandroid.ui.navigation

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.CenterAlignedTopAppBar
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationBarItemDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import dev.luksandroid.ui.browser.BrowserScreen
import dev.luksandroid.ui.browser.viewer.MediaPlayerScreen
import dev.luksandroid.ui.components.SessionStatusHeader
import dev.luksandroid.ui.devices.DevicesScreen
import dev.luksandroid.ui.diagnostics.DiagnosticsScreen
import dev.luksandroid.ui.transfers.TransfersScreen

data class ActiveMedia(val fullPath: String, val displayName: String)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LuksAppNavigation(
    modifier: Modifier = Modifier,
) {
    var currentScreen by remember { mutableStateOf<Screen>(Screen.Devices) }
    var activeMedia by remember { mutableStateOf<ActiveMedia?>(null) }

    Box(modifier = modifier.fillMaxSize()) {
        Scaffold(
            modifier = Modifier.fillMaxSize(),
            topBar = {
                if (activeMedia == null) {
                    Column(modifier = Modifier.fillMaxWidth()) {
                        CenterAlignedTopAppBar(
                            title = {
                                Text(
                                    text = "LUKS Android",
                                    style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                                )
                            },
                            colors = TopAppBarDefaults.topAppBarColors(
                                containerColor = MaterialTheme.colorScheme.background,
                                titleContentColor = MaterialTheme.colorScheme.onBackground,
                            ),
                        )
                        SessionStatusHeader()
                    }
                }
            },
            bottomBar = {
                if (activeMedia == null) {
                    NavigationBar(
                        containerColor = MaterialTheme.colorScheme.surfaceVariant,
                        contentColor = MaterialTheme.colorScheme.onSurfaceVariant,
                    ) {
                        Screen.items.forEach { screen ->
                            val selected = currentScreen == screen
                            NavigationBarItem(
                                selected = selected,
                                onClick = { currentScreen = screen },
                                icon = {
                                    Icon(
                                        imageVector = screen.icon,
                                        contentDescription = screen.title,
                                    )
                                },
                                label = {
                                    Text(
                                        text = screen.title,
                                        style = MaterialTheme.typography.labelMedium,
                                    )
                                },
                                colors = NavigationBarItemDefaults.colors(
                                    selectedIconColor = MaterialTheme.colorScheme.onPrimaryContainer,
                                    selectedTextColor = MaterialTheme.colorScheme.primary,
                                    indicatorColor = MaterialTheme.colorScheme.primaryContainer,
                                    unselectedIconColor = MaterialTheme.colorScheme.onSurfaceVariant,
                                    unselectedTextColor = MaterialTheme.colorScheme.onSurfaceVariant,
                                ),
                            )
                        }
                    }
                }
            },
        ) { innerPadding ->
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(if (activeMedia == null) innerPadding else PaddingValues(0.dp)),
            ) {
                when (currentScreen) {
                    is Screen.Devices -> {
                        DevicesScreen(
                            onNavigateToBrowser = { currentScreen = Screen.Browser },
                        )
                    }
                    is Screen.Browser -> {
                        BrowserScreen(
                            onNavigateToDevices = { currentScreen = Screen.Devices },
                            onPlayMedia = { fullPath, displayName ->
                                activeMedia = ActiveMedia(fullPath, displayName)
                            },
                        )
                    }
                    is Screen.Transfers -> {
                        TransfersScreen()
                    }
                    is Screen.Diagnostics -> {
                        DiagnosticsScreen()
                    }
                }
            }
        }

        activeMedia?.let { media ->
            MediaPlayerScreen(
                fullPath = media.fullPath,
                displayName = media.displayName,
                onClose = { activeMedia = null },
            )
        }
    }
}
