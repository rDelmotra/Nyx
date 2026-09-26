package dev.luksandroid.ui.browser.viewer

import android.app.Activity
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import android.view.ViewTreeObserver
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.ui.Alignment
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.media3.common.MediaItem
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.ui.PlayerView

private val VIDEO_EXTENSIONS = setOf(
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "3gp",
)
private val AUDIO_EXTENSIONS = setOf(
    "mp3", "m4a", "aac", "flac", "wav", "ogg", "opus", "oga",
)

/** Whether [name] looks like a file this screen knows how to play. */
fun isPlayableMedia(name: String): Boolean {
    val ext = name.substringAfterLast('.', "").lowercase()
    return ext in VIDEO_EXTENSIONS || ext in AUDIO_EXTENSIONS
}

private tailrec fun android.content.Context.findActivity(): Activity? = when (this) {
    is Activity -> this
    is ContextWrapper -> baseContext.findActivity()
    else -> null
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MediaPlayerScreen(
    fullPath: String,
    displayName: String,
    onClose: () -> Unit,
) {
    val context = LocalContext.current
    val activity = remember(context) { context.findActivity() }
    val view = LocalView.current
    var isFullscreen by remember { mutableStateOf(false) }

    val player = remember(fullPath) {
        ExoPlayer.Builder(context).build().apply {
            val mediaSource = ProgressiveMediaSource.Factory(LuksDataSource.Factory())
                .createMediaSource(MediaItem.fromUri(LuksDataSource.uriFor(fullPath)))
            setMediaSource(mediaSource)
            prepare()
            playWhenReady = true
        }
    }

    DisposableEffect(player) {
        onDispose { player.release() }
    }

    // Leaving fullscreen on/off is a side effect on the Activity/Window, not
    // Compose state, so it lives in a DisposableEffect rather than being
    // derived — and it must restore both on toggle-off and on leaving the
    // screen entirely, or the whole app is stuck sideways with hidden bars.
    DisposableEffect(isFullscreen) {
        val window = activity?.window
        val insetsController = window?.let { WindowCompat.getInsetsController(it, view) }

        fun hideBars() {
            insetsController?.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            insetsController?.hide(WindowInsetsCompat.Type.systemBars())
        }

        // hide() is not sticky across a window-focus change: a system dialog
        // (e.g. the POST_NOTIFICATIONS prompt) or the notification shade
        // taking focus silently restores the bars, and Android does not
        // re-hide them on its own once focus returns — the caller has to.
        // Scoped to a View-level ViewTreeObserver listener (rather than
        // Activity.onWindowFocusChanged) so it only applies while this
        // screen is actually in fullscreen, not app-wide.
        val focusListener = ViewTreeObserver.OnWindowFocusChangeListener { hasFocus ->
            if (hasFocus && isFullscreen) hideBars()
        }

        if (isFullscreen) {
            // Plain LANDSCAPE, not SENSOR_LANDSCAPE: the sensor variant still
            // needs a live accelerometer/rotation-vector reading to pick
            // normal-vs-reverse landscape, and simply never resolves on an
            // emulator (or a device) that doesn't feed it one — it can hang
            // in the current orientation forever. Plain LANDSCAPE is an
            // unconditional lock with no sensor dependency, so it rotates
            // deterministically regardless of emulator sensor support or the
            // device's auto-rotate setting.
            activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
            hideBars()
            view.viewTreeObserver.addOnWindowFocusChangeListener(focusListener)
        }
        onDispose {
            view.viewTreeObserver.removeOnWindowFocusChangeListener(focusListener)
            activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
            insetsController?.show(WindowInsetsCompat.Type.systemBars())
        }
    }

    if (isFullscreen) {
        Box(modifier = Modifier.fillMaxSize()) {
            AndroidView(
                modifier = Modifier.fillMaxSize(),
                factory = { ctx ->
                    PlayerView(ctx).apply {
                        this.player = player
                        useController = true
                        setFullscreenButtonClickListener { isFullscreen = false }
                        setFullscreenButtonState(true)
                    }
                },
                update = { it.setFullscreenButtonState(true) },
            )
            // Not relying solely on PlayerView's own fullscreen icon here — it
            // sits inside the auto-hiding transport controls, easy to miss or
            // to have tapped-through by a controller-visibility timing quirk.
            // This is a second, always-present way out of fullscreen.
            TextButton(
                onClick = { isFullscreen = false },
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(12.dp),
            ) {
                Text("Exit fullscreen", color = Color.White)
            }
        }
    } else {
        Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text(displayName, maxLines = 1) },
                    navigationIcon = {
                        IconButton(onClick = onClose) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Close")
                        }
                    },
                    actions = {
                        TextButton(onClick = { isFullscreen = true }) {
                            Text("Fullscreen")
                        }
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = MaterialTheme.colorScheme.background,
                    ),
                )
            },
        ) { innerPadding ->
            Box(modifier = Modifier.fillMaxSize()) {
                AndroidView(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(innerPadding),
                    factory = { ctx ->
                        PlayerView(ctx).apply {
                            this.player = player
                            useController = true
                            setFullscreenButtonClickListener { isFullscreen = true }
                            setFullscreenButtonState(false)
                        }
                    },
                    update = { it.setFullscreenButtonState(false) },
                )
            }
        }
    }
}
