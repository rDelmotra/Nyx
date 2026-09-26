package dev.luksandroid.streaming

import android.content.Context
import android.content.Intent
import android.net.Uri
import dev.luksandroid.Trace
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionController
import dev.luksandroid.ui.browser.viewer.inferMimeType
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketException
import java.net.URLEncoder
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors

/**
 * Ephemeral localhost HTTP server (RFC 7233 range support) so an external
 * player (VLC, MX Player) can stream a file from the unlocked volume without
 * it ever being extracted to plaintext on device storage.
 *
 * Not currently reachable from any UI — [launchExternalPlayer] has no caller.
 *
 * Binds to 127.0.0.1 on an OS-assigned port, gated by a per-stream UUID
 * token generated in [registerStream]. That token has no expiry and isn't
 * single-use: it's valid for as long as the server is running, i.e. until
 * [stop] is called (wired to volume lock / USB detach in LuksSession). Any
 * other app on the device that obtains the token can read the stream for as
 * long as it's live — the token is the only access control there is.
 */
object LocalHttpStreamServer {

    var session: SessionController = LuksSession

    private var serverSocket: ServerSocket? = null
    private var serverJob: Job? = null
    private val serverScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private val workerPool = Executors.newCachedThreadPool()

    private val activeSessions = ConcurrentHashMap<String, StreamSession>()

    data class StreamSession(
        val token: String,
        val path: String,
        val mimeType: String,
        val totalSize: Long,
    )

    val isRunning: Boolean
        get() = serverSocket?.isClosed == false

    val port: Int
        get() = serverSocket?.localPort ?: -1

    @Synchronized
    fun start(): Int {
        val currentSocket = serverSocket
        if (currentSocket != null && !currentSocket.isClosed) {
            return currentSocket.localPort
        }

        val socket = ServerSocket(0, 50, InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1)))
        serverSocket = socket

        serverJob = serverScope.launch {
            while (isActive && !socket.isClosed) {
                try {
                    val client = socket.accept()
                    workerPool.execute { handleClient(client) }
                } catch (_: SocketException) {
                    break
                } catch (e: Exception) {
                    if (socket.isClosed) break
                    Trace.e("LocalHttpStreamServer accept error: ${e.message}")
                }
            }
        }
        Trace.i("LocalHttpStreamServer: listening on 127.0.0.1:${socket.localPort}")
        return socket.localPort
    }

    @Synchronized
    fun stop() {
        try {
            serverSocket?.close()
        } catch (_: Exception) {}
        serverSocket = null
        serverJob?.cancel()
        activeSessions.clear()
        Trace.i("LocalHttpStreamServer: stopped")
    }

    fun registerStream(
        path: String,
        mimeType: String,
        totalSize: Long,
    ): String {
        start()
        val token = UUID.randomUUID().toString()
        activeSessions[token] = StreamSession(
            token = token,
            path = path,
            mimeType = mimeType,
            totalSize = totalSize,
        )
        val fileName = path.substringAfterLast('/')
        val encodedName = URLEncoder.encode(fileName, "UTF-8")
        return "http://127.0.0.1:$port/media/$token/$encodedName"
    }

    private fun handleClient(socket: Socket) {
        socket.use { s ->
            s.soTimeout = 15000
            val input = BufferedReader(InputStreamReader(s.getInputStream(), Charsets.US_ASCII))
            val output = s.getOutputStream()

            val requestLine = input.readLine() ?: return
            val parts = requestLine.split(" ")
            if (parts.size < 2) return

            val method = parts[0]
            val uri = parts[1]

            // Parse HTTP headers
            var rangeHeader: String? = null
            var line: String? = input.readLine()
            while (!line.isNullOrBlank()) {
                if (line.startsWith("Range:", ignoreCase = true)) {
                    rangeHeader = line.substring(6).trim()
                }
                line = input.readLine()
            }

            // Route matching: /media/<token>/<filename>
            val pathPart = uri.substringBefore('?')
            val trimmed = pathPart.removePrefix("/media/")
            val token = trimmed.substringBefore('/')
            val session = activeSessions[token]

            if (session == null) {
                writeResponse(output, "HTTP/1.1 404 Not Found", "text/plain", "Invalid or expired stream session\r\n".toByteArray())
                return
            }

            val totalSize = session.totalSize

            if (rangeHeader != null && rangeHeader.startsWith("bytes=")) {
                val rangeSpec = rangeHeader.removePrefix("bytes=").trim()
                val (start, end) = parseRange(rangeSpec, totalSize)
                val contentLength = end - start + 1

                val headers = "HTTP/1.1 206 Partial Content\r\n" +
                        "Content-Type: ${session.mimeType}\r\n" +
                        "Accept-Ranges: bytes\r\n" +
                        "Content-Range: bytes $start-$end/$totalSize\r\n" +
                        "Content-Length: $contentLength\r\n" +
                        "Connection: close\r\n\r\n"

                output.write(headers.toByteArray(Charsets.US_ASCII))
                if (method.equals("HEAD", ignoreCase = true)) return

                streamContent(session.path, start, end, output)
            } else {
                val headers = "HTTP/1.1 200 OK\r\n" +
                        "Content-Type: ${session.mimeType}\r\n" +
                        "Accept-Ranges: bytes\r\n" +
                        "Content-Length: $totalSize\r\n" +
                        "Connection: close\r\n\r\n"

                output.write(headers.toByteArray(Charsets.US_ASCII))
                if (method.equals("HEAD", ignoreCase = true)) return

                streamContent(session.path, 0L, totalSize - 1, output)
            }
        }
    }

    private fun parseRange(rangeSpec: String, totalSize: Long): Pair<Long, Long> {
        val parts = rangeSpec.split("-")
        var start = parts[0].toLongOrNull() ?: 0L
        var end = if (parts.size > 1 && parts[1].isNotBlank()) parts[1].toLongOrNull() ?: (totalSize - 1) else totalSize - 1

        if (start < 0) start = 0
        if (end >= totalSize) end = totalSize - 1
        if (start > end) start = end
        return Pair(start, end)
    }

    private fun streamContent(
        path: String,
        startOffset: Long,
        endOffset: Long,
        output: OutputStream,
    ) {
        var currentOffset = startOffset
        val chunkSize = 256 * 1024 // 256 KiB coalesced streaming chunks

        while (currentOffset <= endOffset) {
            val toRead = minOf(chunkSize.toLong(), endOffset - currentOffset + 1).toInt()
            val chunk = try {
                runBlocking {
                    session.withLease { volume ->
                        volume.readChunk(path, currentOffset, toRead)
                    }
                }
            } catch (_: Exception) {
                break
            }

            if (chunk.isEmpty()) break

            try {
                output.write(chunk)
                output.flush()
            } catch (_: Exception) {
                // Client closed socket or scrubbed forward
                break
            }
            currentOffset += chunk.size
        }
    }

    private fun writeResponse(
        output: OutputStream,
        status: String,
        contentType: String,
        body: ByteArray,
    ) {
        val headers = "$status\r\n" +
                "Content-Type: $contentType\r\n" +
                "Content-Length: ${body.size}\r\n" +
                "Connection: close\r\n\r\n"
        output.write(headers.toByteArray(Charsets.US_ASCII))
        output.write(body)
        output.flush()
    }

    /**
     * Launches external media player (VLC, MX Player, etc.) via Android Intent.
     */
    fun launchExternalPlayer(
        context: Context,
        fullPath: String,
        displayName: String,
        session: SessionController = LuksSession,
    ) {
        val size = try {
            runBlocking { session.withLease { v -> v.fileInfo(fullPath).size } }
        } catch (_: Exception) {
            0L
        }
        val mimeType = inferMimeType(fullPath) ?: "video/*"
        val streamUrl = registerStream(fullPath, mimeType, size)

        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(Uri.parse(streamUrl), mimeType)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }

        val chooser = Intent.createChooser(intent, "Play with...")
        chooser.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(chooser)
    }
}
