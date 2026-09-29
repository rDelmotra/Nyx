package dev.luksandroid.ui

import android.text.Editable
import android.text.InputFilter
import android.text.InputType
import android.text.TextWatcher
import android.text.method.PasswordTransformationMethod
import android.view.ActionMode
import android.view.HapticFeedbackConstants
import android.view.Menu
import android.view.MenuItem
import android.view.View
import android.widget.EditText
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.path
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import dev.luksandroid.security.PassphraseScrubber
import dev.luksandroid.ui.theme.StateUnlocked

private val EyeIcon: ImageVector by lazy {
    ImageVector.Builder(
        name = "Eye",
        defaultWidth = 24.dp,
        defaultHeight = 24.dp,
        viewportWidth = 24f,
        viewportHeight = 24f,
    ).apply {
        path(
            fill = SolidColor(Color.White),
            fillAlpha = 1.0f,
            stroke = null,
            strokeAlpha = 1.0f,
            strokeLineWidth = 1.0f,
            strokeLineCap = StrokeCap.Butt,
            strokeLineJoin = StrokeJoin.Miter,
            strokeLineMiter = 1.0f,
            pathFillType = PathFillType.NonZero,
        ) {
            moveTo(12.0f, 4.5f)
            curveTo(7.0f, 4.5f, 2.73f, 7.61f, 1.0f, 12.0f)
            curveToRelative(1.73f, 4.39f, 6.0f, 7.5f, 11.0f, 7.5f)
            reflectiveCurveToRelative(9.27f, -3.11f, 11.0f, -7.5f)
            curveToRelative(-1.73f, -4.39f, -6.0f, -7.5f, -11.0f, -7.5f)
            close()
            moveTo(12.0f, 17.0f)
            curveToRelative(-2.76f, 0.0f, -5.0f, -2.24f, -5.0f, -5.0f)
            reflectiveCurveToRelative(2.24f, -5.0f, 5.0f, -5.0f)
            reflectiveCurveToRelative(5.0f, 2.24f, 5.0f, 5.0f)
            reflectiveCurveToRelative(-2.24f, 5.0f, -5.0f, 5.0f)
            close()
            moveTo(12.0f, 9.0f)
            curveToRelative(-1.66f, 0.0f, -3.0f, 1.34f, -3.0f, 3.0f)
            reflectiveCurveToRelative(1.34f, 3.0f, 3.0f, 3.0f)
            reflectiveCurveToRelative(3.0f, -1.34f, 3.0f, -3.0f)
            reflectiveCurveToRelative(-1.34f, -3.0f, -3.0f, -3.0f)
            close()
        }
    }.build()
}

@Composable
fun SecurePassphraseField(
    onEditableReady: (Editable) -> Unit,
    onHasContentChange: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    val view = LocalView.current
    val textColor = MaterialTheme.colorScheme.onSurface.toArgb()
    val hintColor = MaterialTheme.colorScheme.onSurfaceVariant.toArgb()

    val noActionMode = remember {
        object : ActionMode.Callback {
            override fun onCreateActionMode(mode: ActionMode?, menu: Menu?): Boolean = false
            override fun onPrepareActionMode(mode: ActionMode?, menu: Menu?): Boolean = false
            override fun onActionItemClicked(mode: ActionMode?, item: MenuItem?): Boolean = false
            override fun onDestroyActionMode(mode: ActionMode?) {}
        }
    }

    var activeEditable: Editable? = remember { null }
    var editTextRef by remember { mutableStateOf<EditText?>(null) }
    var isRevealed by remember { mutableStateOf(false) }

    LaunchedEffect(isRevealed) {
        editTextRef?.let { et ->
            val selStart = et.selectionStart
            val selEnd = et.selectionEnd
            et.transformationMethod = if (isRevealed) null else PasswordTransformationMethod.getInstance()
            et.setSelection(selStart.coerceAtLeast(0), selEnd.coerceAtLeast(0))
        }
    }

    Box(
        modifier = modifier
            .fillMaxWidth()
            .border(
                width = 1.dp,
                color = if (isRevealed) StateUnlocked else MaterialTheme.colorScheme.outline,
                shape = RoundedCornerShape(8.dp)
            )
            .padding(horizontal = 12.dp, vertical = 6.dp)
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically
        ) {
            AndroidView(
                factory = { context ->
                    EditText(context).apply {
                        setEditableFactory(object : Editable.Factory() {
                            override fun newEditable(source: CharSequence): Editable {
                                return PassphraseScrubber.newPreSizedEditable().also { it.append(source) }
                            }
                        })

                        filters = arrayOf(InputFilter.LengthFilter(PassphraseScrubber.MAX_PASSPHRASE_CHARS))
                        inputType = InputType.TYPE_CLASS_TEXT or
                                InputType.TYPE_TEXT_VARIATION_PASSWORD or
                                InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                                0x01000000 // InputType.TYPE_TEXT_FLAG_NO_PERSONALIZED_LEARNING
                        transformationMethod = PasswordTransformationMethod.getInstance()

                        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS
                        isSaveEnabled = false
                        background = null // transparent, styled by parent Box
                        setTextColor(textColor)
                        setHintTextColor(hintColor)
                        hint = "Passphrase"
                        maxLines = 1

                        customSelectionActionModeCallback = noActionMode
                        customInsertionActionModeCallback = noActionMode

                        val currentEditable = text
                        activeEditable = currentEditable
                        if (currentEditable != null) {
                            onEditableReady(currentEditable)
                        }

                        addTextChangedListener(object : TextWatcher {
                            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                            override fun afterTextChanged(s: Editable?) {
                                onHasContentChange(s?.isNotEmpty() == true)
                            }
                        })
                        editTextRef = this
                    }
                },
                modifier = Modifier.weight(1f)
            )

            // Hold-to-reveal peek button
            Box(
                modifier = Modifier
                    .size(36.dp)
                    .pointerInput(Unit) {
                        awaitEachGesture {
                            awaitFirstDown(requireUnconsumed = false)
                            view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                            isRevealed = true
                            waitForUpOrCancellation()
                            view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                            isRevealed = false
                        }
                    },
                contentAlignment = Alignment.Center
            ) {
                Icon(
                    imageVector = EyeIcon,
                    contentDescription = "Hold to reveal passphrase",
                    tint = if (isRevealed) StateUnlocked else MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f),
                    modifier = Modifier.size(20.dp)
                )
            }
        }
    }

    DisposableEffect(Unit) {
        onDispose {
            activeEditable?.let { PassphraseScrubber.scrub(it) }
        }
    }
}
