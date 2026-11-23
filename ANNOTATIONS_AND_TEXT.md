## Play and pause your finished MP4

After saving a screen recording, use **▶ Play** and **⏸ Pause** in the video
preview area to review footage. **↺ Replay** restarts the clip, and the
**Seek bar under the video** lets you pause automatically and drag the playhead to another time (or click to jump). The frame loads when you release the mouse. Pause to add, select, or
move annotation text over a specific video frame. The annotations stay on top
of the moving picture during silent preview and are burned into the entire
MP4 when you choose **Export annotated MP4**. The exported file retains the
original audio. Video trimming, moving/timed text, and audio playback within
the editing preview are not implemented in this version.

# NeuroSnip — editing screenshots and finished recordings

## What's changed

The previous annotation dropdown and Shift+drag gesture have been replaced with a small editing toolbar used by both Screenshot and Screen recording modes.

- **Pen**: drag over the selection preview to draw. Size and color are adjustable.
- **Marker**: drag over the preview for a wider, translucent highlighter.
- **Text**: type text in the toolbar, set its color and font size, then click the preview to place it. Click again to place another copy. Text is a separate editable mark until export.
- **Eraser**: click or drag over pen strokes, highlighter marks, or editable text to remove them. The **Eraser px** slider changes the eraser diameter; the blue circle previews its footprint. Each touched annotation is removed **as a whole object**, not partially scrubbed away. A continuous drag counts as one Undo step. You cannot erase pixels of the original screenshot/video or text that was already embedded in a previously exported file.
- **Undo, Redo, Clear edits**: apply to all annotations, including text.

The screenshot preview shows only the selected rectangle. Save or Copy rasterizes the marks onto the image.

## Finished MP4 editing

1. Select **Screen recording**, click **New selection**, then drag a rectangle over the desktop.
2. Record normally, then choose **Stop recording** and save the original MP4.
3. The editor loads the first video frame as a placement guide. Select **T Text**, enter a caption, and click the frame to place it. Pen and marker are also available.
4. Click **Export annotated MP4** and select a new filename. NeuroSnip rasterizes the annotations as a transparent PNG and uses its bundled FFmpeg to composite that layer over the entire video. Audio is copied without re-encoding; video is re-encoded to H.264.
5. The original recording is preserved. The export runs on a separate thread to keep the UI responsive.

**Current limitation**: annotations are stationary and visible for the **entire** exported video. Timed captions and moving text aren't implemented. The editor supports Play/Pause and seeking through a timeline; pause before drawing or erasing. The text rasterizer uses a font installed with Windows (Segoe UI preferred). Avoid using emoji and complex-script text until those fonts/rendering cases are tested.

## Build

The project contains `.github/workflows/windows-msvc.yml`, which builds the Windows executable through GitHub Actions using `x86_64-pc-windows-msvc`.

Keep these paths intact when uploading to GitHub:

- `Cargo.toml` (includes new `fontdue = "0.9"` dependency)
- `src/main.rs`
- `src/selection.rs`
- `src/editing.rs` (**new**)
- `.github/workflows/windows-msvc.yml`

Upload the *contents* of `NeuroSnip_Rust`, not the `NeuroSnip_Rust` folder itself, to your existing repository. Keep existing GitHub files in place unless they're being replaced by an updated copy.

This archive contains Rust source and build configuration, not a compiled Windows `.exe`. A successful Windows/MSVC build still needs to be run for this revision. The previous revision's GitHub Actions build passed; this new revision has not been compiled on Windows yet.

## Move and edit text after placing it (new)

This version keeps each placed text label independent and editable until you save or export.

1. Choose **T Add text**, type a label, and click the screenshot or finished recording preview to place it.
2. Without changing tools, **click a label** to select it. Drag its body to move it, or drag its blue bottom-right resize handle to expand/shrink it. The selection outline is not exported.
3. **Double-click the label itself** to change the words right on the preview (Enter commits, Esc cancels). Change its color in the Selected text panel, which applies immediately.
4. Use **Delete text** to remove the selected label, **Deselect** to close the panel, or **Undo / Redo** to reverse movement, editing, and deletion.
5. **Save image / Copy image** applies the current labels to a screenshot. For a finished recording, choose **Export annotated MP4** to burn the finalized labels into the video.

The same editing gestures work on screenshots and on a finished recording's first-frame preview. Labels remain stationary for the full exported MP4; editing individual frames, timed captions, and motion tracking are not part of this version. Only the editor's in-memory labels are movable; text already flattened into a previously saved PNG/MP4 cannot be selected and edited again as a text object.

The editor includes unit tests for text movement, selection, and Undo/Redo, plus eraser tests covering text deletion, long strokes, multi-mark dragging, and misses. Run `cargo test` on Windows after installing Rust/MSVC.
