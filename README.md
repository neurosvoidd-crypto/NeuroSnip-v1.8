> **Eraser fix:** Use **⌫ Eraser** to click or drag across pen lines, highlighter strokes, and placed text. The eraser now detects complete line segments (not just stored points), has an adjustable size and visible circular preview, and groups each drag into one Undo step. Each touched annotation is removed as a whole object; original screenshot/video pixels and text already embedded in a saved file cannot be erased.

> **In the video editor:** After stopping a recording, the MP4 is previewable directly in NeuroSnip with **Play, Pause, Replay, and a full-width seek bar beneath the video**. Pause on a frame to position and edit movable text or drawings, then export an annotated MP4. Placed text remains selectable, draggable, and editable. See [ANNOTATIONS_AND_TEXT.md](ANNOTATIONS_AND_TEXT.md) for details.

# NeuroSnip v1.8 — Windows screenshot and screen recorder (Rust)

NeuroSnip is a dark-mode Rust/egui desktop app. **Screenshot** and **Screen recording** now use **one identical full-desktop rectangle picker**, instead of selecting from a scaled-down preview.


## No Command Prompt pop-ups (new in v1.7)

NeuroSnip launches its embedded FFmpeg subprocesses with Windows' `CREATE_NO_WINDOW`
flag. This prevents black Command Prompt windows from appearing when you start or
stop a recording, open the finished video, play or seek through it, extract a
frame, or export an annotated MP4. Video and microphone recording, pipe-based
stop/finalization, and automatic MP4 preview are preserved.

**Rebuild the executable** with `Build NeuroSnip.bat` and launch the newly built
`publish\NeuroSnip.exe` for the fix to take effect. The build script itself
is intentionally a visible Command Prompt window; this update only suppresses
unwanted windows while *using the app*.

## Save recordings when you choose (introduced in v1.6)

- **■ Stop recording** finalizes the MP4 and immediately opens it in the built-in editor. **No save dialog appears.**
- Review the recording with **Play/Pause**, use the timeline to seek, and edit text or drawings.
- **💾 Save recording** opens the Save As dialog only when clicked. It saves the *original* recording, without annotations. After saving, you can still preview and annotate it.
- **Export annotated MP4** saves your edited video with text/ink burned into the MP4. The original recording is not overwritten.
- **Discard recording** requires a second confirmation before permanently deleting an unsaved video. A new capture is blocked until you save or discard the pending recording, preventing accidental loss.
- If you close NeuroSnip before saving, the MP4 itself remains on disk under `%LOCALAPPDATA%\NeuroSnip\PendingRecordings`; copy it out with File Explorer if needed. **Unexported text and drawings are not stored in the MP4.**

## Play, pause, and edit finished recordings

1. Record a region as usual, then click **Stop recording**. The MP4 is finalized and opened in the editor **without showing a save dialog**.
2. The finished recording appears in the **same preview panel** used for annotations.
3. Click **▶ Play** to watch, **⏸ Pause** to freeze the current frame, or **↺ Replay** to start from the beginning.
4. Use the **seek bar below the video** like YouTube: click anywhere to jump or grab the playhead and drag it to the time you want. This automatically pauses playback. Release the mouse to decode and display the chosen frame; the displayed time updates while you drag. The track supports keyboard **←/→** to jump 5 seconds and **Home/End** to reach the start/end.
5. While paused, add, move, or edit text and drawings over the video preview. The annotations stay visible during playback.
6. Click **Save recording** when you want to choose a folder and save the original MP4. To include text or drawings, use **Export annotated MP4** to save a separate edited video.

Seeking decodes the new frame when you release the mouse, to avoid launching dozens of FFmpeg processes during a long drag. Playback is decoded with the same embedded FFmpeg used for recording. It runs in a worker thread at up to 15 preview frames per second, with the image scaled down for efficient display. **Playback in the editor is silent**; microphone audio is still retained in the original MP4 and the exported annotated MP4. The current editor places text/drawings at a fixed location for the *entire* video; timeline-specific annotations, trimming, and audio editing aren't available yet.

## Movable and editable text labels

In screenshot mode or after recording an MP4, choose **T Add text** and click the preview
to place a label. **No Move/Edit tool is needed:** click any text label directly,
 drag the label to move it, and drag the blue bottom-right square to resize it.
 Double-click a label to edit its words in place; press Enter to apply or Escape
 to cancel. The selected label's color and font size update automatically in
 the properties panel. **Delete text** removes the selected label; **Undo/Redo**
can reverse editing and movement. A blue selection outline is only an editor aid
and does not appear in screenshots or exported videos. For video, annotations
remain in a fixed position over the entire exported recording.

## Use it

1. Select **▣ Screenshot** or **● Screen recording** in the top bar.
2. Click **＋ New selection**. NeuroSnip hides its main window and displays a dimmed, frozen image of your desktop over all connected displays.
3. **Click and drag** to select the rectangle. The area inside the blue border stays bright. Release the mouse to confirm. Press **Esc** or **right-click** to cancel.
4. In **Screenshot** mode, the selected image appears in the preview. Draw with **Pen/Marker**, or add and edit text, then **Save image** (PNG/JPEG/BMP) or **Copy image**. Shift is not required to draw.
5. In **Screen recording** mode, recording begins when you finish the drag. Return to NeuroSnip and click **■ Stop recording** to finish the MP4, then review/edit the video and click **💾 Save recording** when you want to save it. **Stop does not open a save dialog.**

Both modes run the same `selection::select_area` native Windows selection overlay in `src/selection.rs`. The main app stays dark.

## Build a standalone Windows EXE

1. Install **Rust** (stable toolchain) from https://rustup.rs/.
2. Install **Visual Studio Build Tools** with the **Desktop development with C++** workload (MSVC compiler/linker).
3. Extract this ZIP completely.
4. Double-click **`Build NeuroSnip.bat`**.
5. When it succeeds, run **`publish\NeuroSnip.exe`**.

The build script creates the `assets` folder automatically, downloads FFmpeg into `assets\ffmpeg.exe`, compresses/embeds it in the exe at compile time, and copies the finished app to `publish`. At first recording, the app extracts FFmpeg to `%LOCALAPPDATA%\NeuroSnip\Runtime`. You should not have to separately install FFmpeg when using the build script.

For a quick source-check without an FFmpeg download, run `cargo check --target x86_64-pc-windows-msvc` in the extracted folder after installing the toolchain. In that case, recording needs `ffmpeg.exe` in PATH if you run the app without bundling it.

## How shared selection works

- NeuroSnip briefly hides the main app, takes a fresh snapshot of **all enumerated monitors**, and combines their real pixel coordinates into a single virtual-desktop image. This includes screens to the left or above the primary monitor (negative virtual-desktop origins).
- A native Windows topmost popup fills the combined desktop. It draws a dimmed snapshot, a bright selected area, a blue rectangle, and an area-size label while the mouse is dragged.
- The selected rectangle is returned in *actual screen pixels*, so PNG crop coordinates and FFmpeg recording offsets refer to the same region.
- Only after the overlay closes does the app save or record. Recording dimensions are rounded **down** to even numbers where needed for H.264 instead of extending outside the chosen rectangle.

## Existing features preserved

- Dark egui interface.
- PNG/JPEG/BMP screenshot saving and clipboard copy.
- Pen/highlighter annotations with direct drag, an eraser, and movable/editable text labels.
- Embedded playback of saved MP4s (Play/Pause/Replay/seek) in the existing editing canvas.
- Screen recording with FFmpeg `gdigrab`, H.264 MP4, and optional microphone through an exact DirectShow device name.
- FFmpeg gets up to 15 seconds to finalize the MP4. After Stop, the recording remains under `%LOCALAPPDATA%\NeuroSnip\PendingRecordings` until **Save recording** is clicked. Cancelling or failing the save leaves the MP4 intact.
- Size-focused Rust release profile and embedded compressed FFmpeg.

## Known limitations / testing notes

- The desktop selection overlay is **Windows-only**. The capture/overlay implementation is new and needs a test on a real Windows PC with your monitor layout and display scaling.
- The app window becomes visible again when recording starts. Move it outside the selected rectangle to avoid capturing NeuroSnip itself. Recording controls remain in the main window; there is not yet a floating Stop toolbar or global stop hotkey.
- The desktop is presented as a *frozen, dimmed screenshot* during selection, not as a live transparent glass overlay. This avoids capturing the app's own selection rectangle in the recording.
- Microphone capture requires the exact DirectShow input device name; **system audio** is not yet implemented.
- FFmpeg must support `gdigrab`, `libx264` and (for microphone) DirectShow/AAC.
- An image memory safeguard limits the combined virtual desktop to 80 million pixels.
- **The project has not been built on Windows in this environment**; the Rust and MSVC toolchains are unavailable here. Compilation and actual recording/monitor/DPI behavior must be verified locally.

## Licensing

Review FFmpeg redistribution requirements and retain any required notices for the exact FFmpeg build you package (GPL/LGPL may apply).
