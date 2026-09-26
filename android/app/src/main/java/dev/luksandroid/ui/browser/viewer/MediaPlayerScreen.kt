package dev.luksandroid.ui.browser.viewer

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import android.graphics.BitmapFactory
import android.view.ViewTreeObserver
import android.view.WindowManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.path
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.MimeTypes
import androidx.media3.common.Player
import androidx.media3.exoplayer.DefaultLoadControl
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.ui.PlayerView

private val VIDEO_EXTENSIONS = setOf(
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "3gp",
)
private val AUDIO_EXTENSIONS = setOf(
    "mp3", "m4a", "aac", "flac", "wav", "ogg", "opus", "oga",
)

private val AudioNoteIcon: ImageVector = ImageVector.Builder(
    name = "AudioNote",
    defaultWidth = 24.dp,
    defaultHeight = 24.dp,
    viewportWidth = 24f,
    viewportHeight = 24f,
).apply {
    path(fill = SolidColor(Color.White)) {
        moveTo(12f, 3f)
        verticalLineToRelative(10.55f)
        curveTo(11.41f, 13.21f, 10.74f, 13f, 10f, 13f)
        curveToRelative(-2.21f, 0f, -4f, 1.79f, -4f, 4f)
        reflectiveCurveToRelative(1.79f, 4f, 4f, 4f)
        reflectiveCurveToRelative(4f, -1.79f, 4f, -4f)
        verticalLineTo(7f)
        horizontalLineToRelative(4f)
        verticalLineTo(3f)
        horizontalLineToRelative(-6f)
        close()
    }
}.build()

/** Whether [name] looks like a file this screen knows how to play. */
fun isPlayableMedia(name: String): Boolean {
    val ext = name.substringAfterLast('.', "").lowercase()
    return ext in VIDEO_EXTENSIONS || ext in AUDIO_EXTENSIONS
}

/** Whether [name] is an audio-only format that does not require a video surface. */
fun isAudioOnlyMedia(name: String): Boolean {
    val ext = name.substringAfterLast('.', "").lowercase()
    return ext in AUDIO_EXTENSIONS
}

/** Infer MIME type from file extension to avoid expensive sequential container sniffing. */
fun inferMimeType(path: String): String? = when (path.substringAfterLast('.', "").lowercase()) {
    "mp4", "m4v" -> MimeTypes.VIDEO_MP4
    "mkv" -> MimeTypes.VIDEO_MATROSKA
    "webm" -> MimeTypes.VIDEO_WEBM
    "mp3" -> MimeTypes.AUDIO_MPEG
    "flac" -> MimeTypes.AUDIO_FLAC
    "wav" -> MimeTypes.AUDIO_WAV
    "ogg", "opus", "oga" -> MimeTypes.AUDIO_OGG
    "aac", "m4a" -> MimeTypes.AUDIO_AAC
    else -> null
}

private tailrec fun Context.findActivity(): Activity? = when (this) {
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
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val activity = remember(context) { context.findActivity() }
    val view = LocalView.current
    val isAudioOnly = remember(fullPath) { isAudioOnlyMedia(fullPath) }

    var isFullscreen by remember { mutableStateOf(false) }
    var trackTitle by remember { mutableStateOf(displayName) }
    var trackArtist by remember { mutableStateOf<String?>(null) }
    var trackAlbum by remember { mutableStateOf<String?>(null) }
    var artworkBytes by remember { mutableStateOf<ByteArray?>(null) }
    var isPlaying by remember { mutableStateOf(false) }

    // Intercept hardware/gesture Back to exit fullscreen first
    BackHandler(enabled = isFullscreen) {
        isFullscreen = false
    }

    // Keep screen awake during active video playback
    DisposableEffect(isPlaying, isAudioOnly) {
        val window = activity?.window
        if (isPlaying && !isAudioOnly) {
            window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
        onDispose {
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
    }

    val player = remember(fullPath) {
        // 30-60s buffer window: fewer, larger native reads than the default,
        // at the cost of more RAM held per open file.
        val loadControl = DefaultLoadControl.Builder()
            .setBufferDurationsMs(
                /* minBufferMs = */ 30_000,
                /* maxBufferMs = */ 60_000,
                /* bufferForPlaybackMs = */ 2_500,
                /* bufferForPlaybackAfterRebufferMs = */ 5_000,
            )
            .setPrioritizeTimeOverSizeThresholds(true)
            .build()

        ExoPlayer.Builder(context)
            .setLoadControl(loadControl)
            .build().apply {
                // Configure proper audio focus and usage
                val audioAttrs = AudioAttributes.Builder()
                    .setUsage(C.USAGE_MEDIA)
                    .setContentType(if (isAudioOnly) C.AUDIO_CONTENT_TYPE_MUSIC else C.AUDIO_CONTENT_TYPE_MOVIE)
                    .build()
                setAudioAttributes(audioAttrs, /* handleAudioFocus= */ true)

                // Build MediaItem with explicit MIME acceleration
                val mediaItemBuilder = MediaItem.Builder()
                    .setUri(LuksDataSource.uriFor(fullPath))
                inferMimeType(fullPath)?.let { mediaItemBuilder.setMimeType(it) }

                val mediaSource = ProgressiveMediaSource.Factory(LuksDataSource.Factory())
                    .createMediaSource(mediaItemBuilder.build())
                setMediaSource(mediaSource)

                addListener(object : Player.Listener {
                    override fun onMediaMetadataChanged(mediaMetadata: MediaMetadata) {
                        mediaMetadata.title?.let { trackTitle = it.toString() }
                        trackArtist = mediaMetadata.artist?.toString()
                        trackAlbum = mediaMetadata.albumTitle?.toString()
                        artworkBytes = mediaMetadata.artworkData
                    }

                    override fun onIsPlayingChanged(playing: Boolean) {
                        isPlaying = playing
                    }
                })

                prepare()
                playWhenReady = true
            }
    }

    DisposableEffect(player) {
        onDispose { player.release() }
    }

    // Fullscreen system bars and orientation management (Video Only)
    DisposableEffect(isFullscreen, isAudioOnly) {
        if (isAudioOnly) return@DisposableEffect onDispose {}

        val window = activity?.window
        val insetsController = window?.let { WindowCompat.getInsetsController(it, view) }

        fun hideBars() {
            insetsController?.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            insetsController?.hide(WindowInsetsCompat.Type.systemBars())
        }

        val focusListener = ViewTreeObserver.OnWindowFocusChangeListener { hasFocus ->
            if (hasFocus && isFullscreen) hideBars()
        }

        if (isFullscreen) {
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

    Scaffold(
        modifier = modifier.fillMaxSize(),
        topBar = {
            if (!isFullscreen) {
                TopAppBar(
                    title = { Text(displayName, maxLines = 1) },
                    navigationIcon = {
                        IconButton(onClick = onClose) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Close")
                        }
                    },
                    actions = {
                        // External-player handoff (LocalHttpStreamServer) is
                        // implemented but not wired to any UI entry point yet —
                        // it opens a new local attack surface (loopback HTTP,
                        // token-gated) and hasn't been decided on.
                        if (!isAudioOnly) {
                            TextButton(onClick = { isFullscreen = true }) {
                                Text("Fullscreen")
                            }
                        }
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = MaterialTheme.colorScheme.background,
                    ),
                )
            }
        },
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .then(if (!isFullscreen) Modifier.padding(innerPadding) else Modifier)
                .background(Color.Black),
            contentAlignment = Alignment.Center,
        ) {
            if (isAudioOnly) {
                // Audio UX: Metadata, Album Art, and Transport Controls (No empty video surface)
                Column(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(24.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.Center,
                ) {
                    val bitmap = remember(artworkBytes) {
                        artworkBytes?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
                    }
                    if (bitmap != null) {
                        Image(
                            bitmap = bitmap.asImageBitmap(),
                            contentDescription = "Cover Art",
                            modifier = Modifier
                                .size(240.dp)
                                .clip(RoundedCornerShape(16.dp)),
                        )
                    } else {
                        Box(
                            modifier = Modifier
                                .size(240.dp)
                                .clip(RoundedCornerShape(16.dp))
                                .background(MaterialTheme.colorScheme.surfaceVariant),
                            contentAlignment = Alignment.Center,
                        ) {
                            Icon(
                                AudioNoteIcon,
                                contentDescription = null,
                                modifier = Modifier.size(96.dp),
                                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }

                    Spacer(modifier = Modifier.height(24.dp))
                    Text(
                        trackTitle,
                        style = MaterialTheme.typography.titleLarge,
                        color = Color.White,
                        maxLines = 1,
                    )
                    trackArtist?.let {
                        Text(
                            it,
                            style = MaterialTheme.typography.bodyMedium,
                            color = Color.LightGray,
                            maxLines = 1,
                        )
                    }
                    trackAlbum?.let {
                        Text(
                            it,
                            style = MaterialTheme.typography.bodySmall,
                            color = Color.Gray,
                            maxLines = 1,
                        )
                    }

                    Spacer(modifier = Modifier.height(32.dp))

                    AndroidView(
                        modifier = Modifier
                            .fillMaxWidth()
                            .height(110.dp),
                        factory = { ctx ->
                            PlayerView(ctx).apply {
                                this.player = player
                                useController = true
                                controllerAutoShow = true
                                controllerShowTimeoutMs = 0
                                controllerHideOnTouch = false
                                setShutterBackgroundColor(android.graphics.Color.TRANSPARENT)
                            }
                        },
                    )
                }
            } else {
                // Video UX: Single persistent PlayerView across windowed and fullscreen states
                AndroidView(
                    modifier = Modifier.fillMaxSize(),
                    factory = { ctx ->
                        PlayerView(ctx).apply {
                            this.player = player
                            useController = true
                            setFullscreenButtonClickListener { isFullscreen = it }
                        }
                    },
                    update = { playerView ->
                        playerView.setFullscreenButtonState(isFullscreen)
                    },
                )

                if (isFullscreen) {
                    TextButton(
                        onClick = { isFullscreen = false },
                        modifier = Modifier
                            .align(Alignment.TopEnd)
                            .padding(16.dp),
                    ) {
                        Text("Exit fullscreen", color = Color.White)
                    }
                }
            }
        }
    }
}
