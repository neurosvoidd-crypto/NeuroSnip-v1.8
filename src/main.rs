#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use image::{DynamicImage, ImageFormat, RgbaImage};
use screenshots::Screen;
use std::{fs, io::Write, path::{Path, PathBuf}, process::{Child, Stdio}, sync::mpsc, thread, time::Duration};

mod selection;
mod editing;
mod playback;
mod ffmpeg_process;
use ffmpeg_process::hidden_ffmpeg;
use editing::{Editor, Tool};

const EMBEDDED_FFMPEG_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.exe.gz"));

#[derive(Clone, Copy, PartialEq)]
enum Mode { Screenshot, Recording }

struct Recorder { child: Child, temp_path: PathBuf }

struct NeuroSnip {
    screen: Option<RgbaImage>,
    screen_origin: (i32, i32),
    selected: Option<[u32; 4]>,
    mode: Mode,
    texture: Option<egui::TextureHandle>,
    editor: Editor,
    // Until Save recording is clicked, the finalized MP4 stays on disk in
    // %LOCALAPPDATA%/NeuroSnip/PendingRecordings and is editable in-place.
    pending_video: Option<PathBuf>,
    confirm_discard: bool,
    saved_video: Option<PathBuf>,
    video_preview: Option<playback::VideoPreview>,
    export_job: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    recorder: Option<Recorder>,
    microphone: bool,
    microphone_name: String,
    status: String,
    ffmpeg_path: Option<PathBuf>,
}

impl Default for NeuroSnip {
    fn default() -> Self {
        Self { screen: None, screen_origin: (0, 0), selected: None,
            mode: Mode::Screenshot, texture: None, editor: Editor::default(),
            pending_video: None, confirm_discard: false,
            saved_video: None, video_preview: None, export_job: None, recorder: None,
            microphone: false, microphone_name: String::new(), status: "Ready".into(), ffmpeg_path: None }
    }
}

impl NeuroSnip {
    /// Capture all displays into one image, preserving their virtual-desktop
    /// coordinates (including monitors placed left/above the primary display).
    fn capture_screen(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let screens = Screen::all().map_err(|e| format!("Cannot enumerate monitors: {e}"))?;
        if screens.is_empty() { return Err("No monitor was found".into()); }
        let left = screens.iter().map(|s| s.display_info.x).min().unwrap();
        let top = screens.iter().map(|s| s.display_info.y).min().unwrap();
        let right = screens.iter().map(|s| i64::from(s.display_info.x) + i64::from(s.display_info.width)).max().unwrap();
        let bottom = screens.iter().map(|s| i64::from(s.display_info.y) + i64::from(s.display_info.height)).max().unwrap();
        let width = u32::try_from(right - i64::from(left)).map_err(|e| e.to_string())?;
        let height = u32::try_from(bottom - i64::from(top)).map_err(|e| e.to_string())?;
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 80_000_000 {
            return Err("The desktop area is too large to capture safely".into());
        }
        let mut combined = RgbaImage::new(width, height);
        for screen in screens {
            let shot = screen.capture().map_err(|e| format!("Monitor capture failed: {e}"))?;
            // screenshots uses a different image crate version; copy raw RGBA pixels.
            let (w, h) = (shot.width(), shot.height());
            let pixels = shot.into_raw();
            let x = u32::try_from(screen.display_info.x - left).map_err(|e| e.to_string())?;
            let y = u32::try_from(screen.display_info.y - top).map_err(|e| e.to_string())?;
            if x + w > width || y + h > height {
                return Err("Monitor layout changed during capture; please try again".into());
            }
            let row_size = w as usize * 4;
            let combined_bytes: &mut [u8] = combined.as_mut();
            for row in 0..h as usize {
                let dst = ((y as usize + row) * width as usize + x as usize) * 4;
                combined_bytes[dst..dst + row_size]
                    .copy_from_slice(&pixels[row * row_size..(row + 1) * row_size]);
            }
        }
        self.screen_origin = (left, top);
        self.screen = Some(combined);
        self.selected = None;
        self.editor.reset();
        self.refresh_texture(ctx);
        Ok(())
    }

    /// Both UI modes call this *same* Windows desktop rectangle picker.
    fn begin_selection(&mut self, ctx: &egui::Context) {
        if self.recorder.is_some() {
            self.status = "Stop recording before making another selection".into();
            return;
        }
        if self.pending_video.is_some() {
            self.status = "Save or discard your finished recording before starting a new selection".into();
            return;
        }
        if let Some(player) = self.video_preview.as_mut() { player.pause(); }
        // Hide NeuroSnip while taking the background snapshot, so the picker
        // shows the real desktop rather than the app's own window.
        let hidden = selection::HiddenApp::new();
        thread::sleep(Duration::from_millis(160));
        if let Err(error) = self.capture_screen(ctx) {
            self.status = error;
            return; // HiddenApp's Drop restores the application.
        }
        let mode_label = match self.mode { Mode::Screenshot => "Screenshot", Mode::Recording => "Recording" };
        let result = selection::select_area(self.screen.as_ref().unwrap(), self.screen_origin, mode_label);
        drop(hidden);
        match result {
            Ok(Some(area)) => {
                self.selected = Some(area);
                self.editor.reset();
                self.saved_video = None;
                self.video_preview = None;
                self.refresh_texture(ctx);
                match self.mode {
                    Mode::Screenshot => {
                        self.status = format!("Screenshot area: {} × {}. Save or copy your image.", area[2], area[3]);
                    }
                    Mode::Recording => self.start_recording(),
                }
            }
            Ok(None) => {
                self.selected = None;
                self.status = "Selection cancelled (Esc or right-click)".into();
            }
            Err(error) => {
                self.selected = None;
                self.status = format!("Desktop selection failed: {error}");
            }
        }
        ctx.request_repaint();
    }

    fn refresh_texture(&mut self, ctx: &egui::Context) {
        // The edit canvas is the selected region, not the entire virtual desktop.
        // Consequently screenshot and video annotations use the same 0..1 coords.
        if let Some(img) = self.selected_image() {
            let longest = img.width().max(img.height());
            let preview = if longest > 4096 {
                let scale = 4096.0_f32 / longest as f32;
                image::imageops::resize(&img,
                    (img.width() as f32 * scale).round().max(1.0) as u32,
                    (img.height() as f32 * scale).round().max(1.0) as u32,
                    image::imageops::FilterType::Triangle)
            } else { img };
            let size = [preview.width() as usize, preview.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, preview.as_raw());
            self.texture = Some(ctx.load_texture("capture-preview", color, egui::TextureOptions::LINEAR));
        } else { self.texture = None; }
    }

    fn selected_image(&self) -> Option<RgbaImage> {
        let src = self.screen.as_ref()?;
        let (x, y, w, h) = match self.selected {
            Some([x, y, w, h]) => (x, y, w, h),
            None => (0, 0, src.width(), src.height()),
        };
        if w == 0 || h == 0 || x + w > src.width() || y + h > src.height() { return None; }
        Some(image::imageops::crop_imm(src, x, y, w, h).to_image())
    }

    fn save_image(&mut self) {
        let Some(mut img) = self.selected_image() else { self.status = "Capture an area first".into(); return; };
        if let Err(error) = self.apply_edits(&mut img) { self.status = error; return; }
        let Some(path) = rfd::FileDialog::new().set_file_name("NeuroSnip.png")
            .add_filter("PNG image", &["png"]).add_filter("JPEG image", &["jpg", "jpeg"])
            .add_filter("Bitmap", &["bmp"]).save_file() else { self.status = "Save cancelled".into(); return; };
        let format = match path.extension().and_then(|x| x.to_str()).unwrap_or("png").to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => ImageFormat::Jpeg, "bmp" => ImageFormat::Bmp, _ => ImageFormat::Png,
        };
        let result = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgba8(img).to_rgb8().save_with_format(&path, format)
        } else {
            DynamicImage::ImageRgba8(img).save_with_format(&path, format)
        };
        self.status = match result { Ok(()) => format!("Saved {}", path.display()), Err(e) => format!("Image save failed: {e}") };
    }

    fn copy_image(&mut self) {
        let Some(mut img) = self.selected_image() else { self.status = "Capture an area first".into(); return; };
        if let Err(error) = self.apply_edits(&mut img) { self.status = error; return; }
        match Clipboard::new().and_then(|mut c| c.set_image(arboard::ImageData {
            width: img.width() as usize, height: img.height() as usize, bytes: img.into_raw().into(),
        })) {
            Ok(()) => self.status = "Copied image to clipboard".into(),
            Err(e) => self.status = format!("Clipboard copy failed: {e}"),
        }
    }

    fn apply_edits(&self, img: &mut RgbaImage) -> Result<(), String> {
        if self.editor.has_edits() {
            let layer = self.editor.render_overlay(img.width(), img.height())?;
            image::imageops::overlay(img, &layer, 0, 0);
        }
        Ok(())
    }

    /// Read a real frame from the saved MP4 for accurate post-recording editing.
    /// If frame extraction fails the pre-recording selection preview remains available.
    fn show_video_frame(&mut self, path: &Path, ctx: &egui::Context) -> Result<(), String> {
        let ffmpeg = self.ensure_ffmpeg()?;
        let dir = std::env::temp_dir().join("NeuroSnip").join("FramePreview");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let frame = dir.join(format!("frame-{}.png", chrono_free_stamp()));
        let output = hidden_ffmpeg(ffmpeg)
            .arg("-hide_banner").arg("-loglevel").arg("error").arg("-y")
            .arg("-i").arg(path).args(["-frames:v", "1"]).arg(&frame)
            .output().map_err(|e| format!("Cannot extract video frame: {e}"))?;
        if !output.status.success() {
            let _ = fs::remove_file(&frame);
            return Err(format!("FFmpeg frame extraction failed: {}",
                String::from_utf8_lossy(&output.stderr)));
        }
        let decoded = image::open(&frame).map_err(|e| e.to_string());
        let _ = fs::remove_file(&frame);
        let image = decoded?.to_rgba8();
        self.selected = Some([0, 0, image.width(), image.height()]);
        self.screen_origin = (0, 0);
        self.screen = Some(image);
        self.refresh_texture(ctx);
        Ok(())
    }

    fn export_video_with_edits(&mut self) {
        if self.export_job.is_some() {
            self.status = "An export is already in progress".into();
            return;
        }
        let Some(source) = self.saved_video.clone() else {
            self.status = "Stop recording before exporting an edited video".into();
            return;
        };
        let Some([_, _, width, height]) = self.selected else {
            self.status = "No video frame is ready for editing".into();
            return;
        };
        if !self.editor.has_edits() {
            self.status = "Add a drawing or text label first, then export".into();
            return;
        }
        let layer = match self.editor.render_overlay(width, height) {
            Ok(layer) => layer,
            Err(error) => { self.status = error; return; }
        };
        let ffmpeg = match self.ensure_ffmpeg() {
            Ok(path) => path,
            Err(error) => { self.status = error; return; }
        };
        let name = format!("{}-annotated.mp4",
            source.file_stem().and_then(|s| s.to_str()).unwrap_or("NeuroSnip-Recording"));
        let Some(target) = rfd::FileDialog::new()
            .add_filter("MP4 video", &["mp4"])
            .set_file_name(&name).save_file() else {
                self.status = "Video export cancelled".into();
                return;
            };
        // FFmpeg must never overwrite the original source input.
        if recording_paths_match(&target, &source) {
            self.status = "Choose a different filename to preserve the original recording".into();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.export_job = Some(rx);
        self.status = "Rendering annotations onto the MP4 — original recording is safe".into();
        thread::spawn(move || {
            let result = render_annotated_video(&ffmpeg, &source, &target, &layer);
            let _ = tx.send(result);
        });
    }

    fn ensure_ffmpeg(&mut self) -> Result<PathBuf, String> {
        if let Some(p) = &self.ffmpeg_path { if p.exists() { return Ok(p.clone()); } }
        if !EMBEDDED_FFMPEG_GZ.is_empty() {
            let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
                .join("NeuroSnip").join("Runtime");
            fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            let exe = root.join("ffmpeg.exe");
            if !exe.exists() {
                let mut decoder = flate2::read::GzDecoder::new(EMBEDDED_FFMPEG_GZ);
                let mut file = fs::File::create(&exe).map_err(|e| e.to_string())?;
                std::io::copy(&mut decoder, &mut file).map_err(|e| e.to_string())?;
            }
            self.ffmpeg_path = Some(exe.clone());
            return Ok(exe);
        }
        self.ffmpeg_path = Some(PathBuf::from("ffmpeg.exe"));
        Ok(PathBuf::from("ffmpeg.exe"))
    }

    fn start_recording(&mut self) {
        if self.pending_video.is_some() {
            self.status = "Save or discard the previous recording before recording again".into();
            return;
        }
        let Some([x, y, w, h]) = self.selected else { self.status = "Select a recording area first".into(); return; };
        if w < 2 || h < 2 { self.status = "Selected area is too small".into(); return; }
        // H.264/YUV420 requires even dimensions. Crop inward, never beyond the selection.
        let (video_w, video_h) = (even_floor(w), even_floor(h));
        if self.microphone && self.microphone_name.trim().is_empty() { self.status = "Enter the exact DirectShow microphone device name".into(); return; }
        let ffmpeg = match self.ensure_ffmpeg() { Ok(p) => p, Err(e) => { self.status = format!("FFmpeg setup failed: {e}"); return; } };
        let temp_dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join("NeuroSnip").join("PendingRecordings");
        if let Err(e) = fs::create_dir_all(&temp_dir) { self.status = format!("Cannot create recording folder: {e}"); return; }
        let stamp = chrono_free_stamp();
        let output = temp_dir.join(format!("Recording-{stamp}.mp4"));
        let mut cmd = hidden_ffmpeg(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "warning", "-y", "-f", "gdigrab", "-framerate", "30",
            "-offset_x", &(x as i32 + self.screen_origin.0).to_string(), "-offset_y", &(y as i32 + self.screen_origin.1).to_string(),
            "-video_size", &format!("{}x{}", video_w, video_h), "-i", "desktop"]);
        if self.microphone {
            cmd.args(["-f", "dshow", "-i", &format!("audio={}", self.microphone_name.trim())]);
            cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
        }
        cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p", "-movflags", "+faststart"]);
        if self.microphone { cmd.args(["-c:a", "aac", "-b:a", "128k"]); }
        cmd.arg(&output).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() { thread::spawn(move || { let mut sink = std::io::sink(); let mut r = stderr; let _ = std::io::copy(&mut r, &mut sink); }); }
                self.recorder = Some(Recorder { child, temp_path: output });
                self.saved_video = None;
                self.video_preview = None;
                self.editor.reset();
                self.status = "Recording… press Stop to finish, then save whenever you're ready".into();
            }
            Err(e) => self.status = format!("Could not start FFmpeg: {e}. Run the build script to bundle FFmpeg."),
        }
    }

    /// Finish FFmpeg's MP4 file without opening a Save dialog. The pending
    /// file remains on disk across application shutdowns until explicitly saved
    /// or discarded. This allows Play/Pause, seeking and editing before saving.
    fn prepare_recording_for_review(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.pending_video = Some(path.clone());
        self.saved_video = Some(path.clone());
        self.confirm_discard = false;
        self.editor.reset();
        let first_frame = self.show_video_frame(&path, ctx);
        if first_frame.is_err() {
            // FFmpeg recorded only even dimensions, so the preview fallback must
            // use the same dimensions as the final MP4.
            if let Some([x, y, w, h]) = self.selected {
                self.selected = Some([x, y, even_floor(w), even_floor(h)]);
                self.refresh_texture(ctx);
            }
        }
        let video_size = self.selected.map(|[_, _, w, h]| [w, h]);
        let preview = self.ensure_ffmpeg().and_then(|ffmpeg| {
            let size = video_size.ok_or_else(|| "No selected video area".to_string())?;
            playback::VideoPreview::open(ffmpeg, path.clone(), size)
        });
        self.status = match preview {
            Ok(player) => {
                self.video_preview = Some(player);
                match first_frame {
                    Ok(()) => "Recording stopped. Preview or edit it, then click Save recording when ready. No save dialog opens automatically.".into(),
                    Err(error) => format!("Recording stopped and kept safely at {}. First-frame preview failed: {error}. Click Save recording when ready.", path.display()),
                }
            }
            Err(error) => format!("Recording stopped and kept safely at {}. Preview unavailable: {error}. You can still click Save recording.", path.display()),
        };
        ctx.request_repaint();
    }

    /// The Save dialog is invoked ONLY by the toolbar's Save recording button.
    /// Saving the original recording does not flatten any annotations; users can
    /// separately export an annotated MP4 from the video editor.
    fn save_recording(&mut self, _ctx: &egui::Context) {
        if self.recorder.is_some() || self.export_job.is_some() {
            self.status = "Stop recording and finish any video export before saving".into();
            return;
        }
        let Some(source) = self.saved_video.clone() else {
            self.status = "There is no finished recording to save".into();
            return;
        };
        if !source.is_file() {
            self.status = format!("Recording could not be found at {}", source.display());
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name("NeuroSnip-Recording.mp4")
            .add_filter("MP4 video", &["mp4"]).save_file() else {
                self.status = "Save cancelled. The recording is still available in the editor.".into();
                return;
            };
        if recording_paths_match(&source, &dest) {
            self.status = "Choose a different destination from the working recording file".into();
            return;
        }
        match fs::copy(&source, &dest) {
            Ok(_) => {
                let pending = self.pending_video.as_ref() == Some(&source);
                // The preview may have an FFmpeg worker reading the old path.
                // Reopen it using the new, user-chosen location before deleting
                // the pending copy, without losing the editing layer or playhead.
                let position = self.video_preview.as_ref().map(|p| p.position()).unwrap_or(0.0);
                self.video_preview = None;
                self.saved_video = Some(dest.clone());
                if let Some([_, _, w, h]) = self.selected {
                    if let Ok(ffmpeg) = self.ensure_ffmpeg() {
                        if let Ok(mut preview) = playback::VideoPreview::open(ffmpeg, dest.clone(), [w, h]) {
                            if position > 0.0 { preview.seek(position); }
                            self.video_preview = Some(preview);
                        }
                    }
                }
                if pending {
                    self.pending_video = None;
                    self.confirm_discard = false;
                    if let Err(error) = fs::remove_file(&source) {
                        self.status = format!("Recording saved to {}. A temporary copy remains at {} ({error}).", dest.display(), source.display());
                        return;
                    }
                }
                self.status = if self.editor.has_edits() {
                    format!("Original recording saved to {}. To include drawings or text, click Export annotated MP4.", dest.display())
                } else {
                    format!("Recording saved to {}", dest.display())
                };
            }
            Err(error) => {
                self.status = format!("Could not save recording: {error}. Original remains at {}", source.display());
            }
        }
    }

    fn discard_recording(&mut self) {
        let Some(path) = self.pending_video.clone() else { return; };
        // Close the preview first so the file can be deleted on Windows.
        self.video_preview = None;
        match fs::remove_file(&path) {
            Ok(()) => {
                self.pending_video = None;
                self.saved_video = None;
                self.confirm_discard = false;
                self.selected = None;
                self.screen = None;
                self.texture = None;
                self.editor.reset();
                self.status = "Unsaved recording discarded. You can start a new selection.".into();
            }
            Err(error) => {
                self.confirm_discard = false;
                self.status = format!("Could not discard the recording: {error}. It is still at {}", path.display());
            }
        }
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Some(mut rec) = self.recorder.take() else { return; };
        if let Some(mut input) = rec.child.stdin.take() {
            let _ = input.write_all(b"q\n");
        }
        // Allow FFmpeg to finalize the MP4 index; do not open any dialogs here.
        let mut exit_status = None;
        for _ in 0..150 {
            match rec.child.try_wait() {
                Ok(Some(status)) => { exit_status = Some(status); break; }
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let Some(status) = exit_status else {
            let _ = rec.child.kill();
            let _ = rec.child.wait();
            self.status = format!("FFmpeg did not finish finalizing within 15 seconds. The file remains at {}", rec.temp_path.display());
            return;
        };
        if !status.success() {
            self.status = format!("Recorder stopped with status {:?}. Any output was preserved at {}", status.code(), rec.temp_path.display());
            return;
        }
        if fs::metadata(&rec.temp_path).map(|m| m.len() < 1024).unwrap_or(true) {
            self.status = format!("Recording did not produce a usable MP4. Temporary file: {}", rec.temp_path.display());
            return;
        }
        self.prepare_recording_for_review(rec.temp_path, ctx);
    }

}

impl eframe::App for NeuroSnip {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(player) = self.video_preview.as_mut() {
            player.tick(ctx);
        }
        // A finished export is reported asynchronously so long recordings do not freeze the UI.
        if let Some(rx) = &self.export_job {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.status = format!("Annotated MP4 saved to {}", path.display());
                    self.export_job = None;
                }
                Ok(Err(error)) => {
                    self.status = format!("Video export failed (original kept): {error}");
                    self.export_job = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Video exporter exited unexpectedly; original kept".into();
                    self.export_job = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("NeuroSnip v1.8");
                ui.separator();
                ui.add_enabled_ui(self.recorder.is_none() && self.export_job.is_none()
                    && self.pending_video.is_none(), |ui| {
                    let previous_mode = self.mode;
                    ui.selectable_value(&mut self.mode, Mode::Screenshot, "▣ Screenshot");
                    ui.selectable_value(&mut self.mode, Mode::Recording, "● Screen recording");
                    if self.mode != previous_mode {
                        if let Some(player) = self.video_preview.as_mut() { player.pause(); }
                    }
                });
                ui.separator();
                if self.recorder.is_some() {
                    if ui.button("■ Stop recording").clicked() { self.stop_recording(ctx); }
                } else if ui.add_enabled(self.export_job.is_none(), egui::Button::new("＋ New selection")).clicked() {
                    self.begin_selection(ctx);
                }
                if self.mode == Mode::Recording && self.recorder.is_none() {
                    ui.checkbox(&mut self.microphone, "Microphone");
                    if self.microphone {
                        ui.add(egui::TextEdit::singleline(&mut self.microphone_name)
                            .hint_text("Exact DirectShow microphone name").desired_width(190.0));
                    }
                }
            });
            // The save dialog is only shown when the user clicks this button.
            // The finalized MP4 is already available to scrub and annotate.
            if self.mode == Mode::Recording && self.recorder.is_none() && self.saved_video.is_some() {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    let title = if self.pending_video.is_some() { "💾 Save recording" } else { "💾 Save copy…" };
                    if ui.add_enabled(self.export_job.is_none(), egui::Button::new(title))
                        .on_hover_text("Save the original MP4 on demand. Use Export annotated MP4 to include text/drawings.")
                        .clicked() {
                        self.save_recording(ctx);
                    }
                    if self.pending_video.is_some() {
                        if self.confirm_discard {
                            ui.colored_label(Color32::LIGHT_RED, "Delete this unsaved recording permanently?");
                            if ui.button("Yes, discard").clicked() { self.discard_recording(); }
                            if ui.button("Keep recording").clicked() { self.confirm_discard = false; }
                        } else if ui.add_enabled(self.export_job.is_none(), egui::Button::new("Discard recording")).clicked() {
                            self.confirm_discard = true;
                        }
                        ui.label("Not saved yet — review, edit, or save when ready.");
                    }
                });
            }

            let ready = self.selected.is_some()
                && (self.mode == Mode::Screenshot || self.saved_video.is_some())
                && self.recorder.is_none();
            if ready {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if self.mode == Mode::Screenshot {
                        if ui.button("Save image").clicked() { self.save_image(); }
                        if ui.button("Copy image").clicked() { self.copy_image(); }
                    } else {
                        if ui.add_enabled(self.export_job.is_none(), egui::Button::new("Export annotated MP4"))
                            .on_hover_text("Burn text and drawings onto the finished recording; keep original MP4")
                            .clicked() {
                            self.export_video_with_edits();
                        }
                    }
                    ui.separator();
                    ui.label("Annotate:");
                    let previous_tool = self.editor.tool;
                    ui.selectable_value(&mut self.editor.tool, Tool::Pen, "✎ Pen");
                    ui.selectable_value(&mut self.editor.tool, Tool::Highlighter, "▰ Marker");
                    ui.selectable_value(&mut self.editor.tool, Tool::Text, "T Add text");
                    ui.selectable_value(&mut self.editor.tool, Tool::Eraser, "⌫ Eraser");
                    if self.editor.tool == Tool::Eraser && self.editor.tool != previous_tool {
                        self.editor.deselect_text();
                    }
                    ui.separator();
                    if matches!(self.editor.tool, Tool::Pen | Tool::Highlighter | Tool::Text) {
                        ui.label("Color");
                        ui.color_edit_button_srgba(&mut self.editor.color);
                    }
                    if matches!(self.editor.tool, Tool::Pen | Tool::Highlighter) {
                        ui.add(egui::Slider::new(&mut self.editor.brush_size, 1.0..=30.0).text("Size"));
                    }
                    if self.editor.tool == Tool::Eraser {
                        ui.add(egui::Slider::new(&mut self.editor.eraser_size, 4.0..=100.0)
                            .text("Eraser px"));
                    }
                    if self.editor.tool == Tool::Text {
                        ui.add(egui::Slider::new(&mut self.editor.text_size, 14.0..=144.0).text("Font px"));
                    }
                });
                if self.editor.tool == Tool::Text {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Text to place:");
                        ui.add(egui::TextEdit::singleline(&mut self.editor.text_input)
                            .hint_text("Type here, then click on the image to place")
                            .desired_width(370.0));
                        ui.label("Click empty space to add; click existing text to select.");
                    });
                }
                if self.editor.has_selected_text() {
                    ui.group(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong("Selected text — drag to move · drag blue corner to resize · double-click to type");
                            if ui.button("Deselect").clicked() { self.editor.deselect_text(); }
                        });
                        if self.editor.has_selected_text() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Color:");
                                let color_changed = ui.color_edit_button_srgba(&mut self.editor.edit_text_color).changed();
                                let size_changed = ui.add(egui::Slider::new(&mut self.editor.edit_text_size, 12.0..=256.0)
                                    .text("Font px")).changed();
                                // Style controls update the selected label instantly.
                                // Double-click the label itself to edit its wording in place.
                                if color_changed || size_changed {
                                    self.status = match self.editor.apply_selected_text() {
                                        Ok(()) => "Text style updated".into(),
                                        Err(error) => error,
                                    };
                                }
                                if ui.add_enabled(self.export_job.is_none(), egui::Button::new("Delete text")).clicked() {
                                    self.editor.delete_selected_text();
                                    self.status = "Text deleted — Undo can restore it".into();
                                }
                            });
                        }
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(self.editor.can_undo(), egui::Button::new("↶ Undo")).clicked() {
                        self.editor.undo();
                    }
                    if ui.add_enabled(self.editor.can_redo(), egui::Button::new("↷ Redo")).clicked() {
                        self.editor.redo();
                    }
                    if ui.add_enabled(self.editor.has_edits(), egui::Button::new("Clear edits")).clicked() {
                        self.editor.clear();
                    }
                    ui.label(match self.editor.tool {
                        Tool::Pen => "Drag on the preview to draw",
                        Tool::Highlighter => "Drag to highlight",
                        Tool::Eraser => "Click or drag over ink, highlights, or text to erase whole annotations; Undo restores them",
                        Tool::Text => "Click empty space to add text; drag any label to move; drag its corner to resize",
                    });
                });
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(match self.mode {
                Mode::Screenshot => "Screenshot: select a desktop area, annotate the image, then Save or Copy. Draw directly — Shift is no longer required.",
                Mode::Recording if self.recorder.is_some() => "Recording in progress. Stop finalizes the MP4 without opening a save dialog.",
                Mode::Recording if self.pending_video.is_some() => "Recording stopped (not saved yet). Use Play/Pause or seek to review and edit, then click Save recording. Export annotated MP4 separately if you want text or drawings burned in.",
                Mode::Recording if self.saved_video.is_some() => "Finished MP4: Play/Pause or scrub below; Save copy saves original video, Export annotated MP4 includes text/drawings.",
                Mode::Recording => "Screen recording: New selection starts the recorder; Stop finalizes it. Save recording only when you choose.",
            });
            if let (Some([_, _, output_w, output_h]), Some(texture)) = (self.selected, &self.texture) {
                let texture_id = if self.mode == Mode::Recording && self.saved_video.is_some() {
                    self.video_preview.as_ref().and_then(|p| p.texture_id()).unwrap_or_else(|| texture.id())
                } else { texture.id() };
                let available = ui.available_size();
                let image_size = Vec2::new(output_w as f32, output_h as f32);
                // Reserve room for a full-width seek bar and playback controls
                // beneath the video rather than squeezing them above it.
                let controls_height = if self.mode == Mode::Recording
                    && self.saved_video.is_some() && self.video_preview.is_some() {
                    100.0
                } else { 0.0 };
                let scale = (available.x / image_size.x)
                    .min((available.y - controls_height - 10.0).max(1.0) / image_size.y).min(1.0);
                let display_size = image_size * scale;
                let (rect, response) = ui.allocate_exact_size(display_size, Sense::click_and_drag());
                ui.painter().image(texture_id, rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, Color32::from_rgb(100, 135, 165)));
                let editable = self.recorder.is_none()
                    && (self.mode == Mode::Screenshot || self.saved_video.is_some())
                    && !self.video_preview.as_ref().is_some_and(|p| p.playing())
                    && self.export_job.is_none();
                if editable {
                    let norm = |pos: Pos2| -> [f32; 2] { [
                        ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
                        ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
                    ] };
                    if response.hovered() {
                        let over_label = response.hover_pos().is_some_and(|pos|
                            self.editor.hovering_text(norm(pos), [output_w, output_h]));
                        let over_resize = response.hover_pos().is_some_and(|pos|
                            self.editor.hovering_resize_handle(norm(pos), [output_w, output_h], scale));
                        let cursor = if over_resize {
                            egui::CursorIcon::ResizeNwSe
                        } else if over_label {
                            egui::CursorIcon::Grab
                        } else { egui::CursorIcon::Crosshair };
                        ui.output_mut(|out| out.cursor_icon = cursor);
                    }
                    if !self.editor.editing_inline() && response.drag_started() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            if rect.contains(pos) {
                                self.editor.begin_drag(norm(pos), [output_w, output_h], scale);
                            }
                        }
                    }
                    if !self.editor.editing_inline() && response.dragged() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            self.editor.continue_drag(norm(pos), [output_w, output_h]);
                        }
                    }
                    if !self.editor.editing_inline() && response.drag_stopped() {
                        // Include the final pointer position: a fast mouse release may
                        // otherwise skip the last stroke or eraser path segment.
                        if let Some(pos) = response.interact_pointer_pos() {
                            self.editor.continue_drag(norm(pos), [output_w, output_h]);
                        }
                        self.editor.end_drag();
                    }
                    if !self.editor.editing_inline() && response.double_clicked() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            if rect.contains(pos) && self.editor.start_inline_edit_at(norm(pos), [output_w, output_h]) {
                                self.status = "Editing text in place — Enter to apply, Esc to cancel".into();
                            }
                        }
                    }
                    if !self.editor.editing_inline() && response.clicked() && !response.double_clicked() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            if rect.contains(pos) {
                                if let Err(error) = self.editor.click(norm(pos), [output_w, output_h]) {
                                    self.status = error;
                                }
                            }
                        }
                    }
                    self.editor.paint_preview(ui, rect, output_w, output_h);
                    if let Some(result) = self.editor.inline_text_ui(ui, rect, [output_w, output_h]) {
                        self.status = match result {
                            Ok(()) => "Text updated — save or export to keep your changes".into(),
                            Err(error) => error,
                        };
                    }
                    // Show the eraser footprint without changing the exported image/video.
                    if self.editor.tool == Tool::Eraser {
                        if let Some(pointer) = response.hover_pos() {
                            ui.painter().circle_stroke(pointer,
                                self.editor.eraser_size * 0.5 * scale,
                                Stroke::new(1.5_f32, Color32::LIGHT_BLUE));
                        }
                    }
                } else if self.mode == Mode::Recording && self.saved_video.is_some() {
                    self.editor.paint_preview(ui, rect, output_w, output_h);
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(90.0);
                    ui.heading("Ready to capture");
                    ui.label("Choose Screenshot or Screen recording and click New selection.");
                });
            }
            // Video controls intentionally appear beneath the preview. Clicking
            // or dragging the seek bar pauses playback, then loads that frame.
            if self.mode == Mode::Recording && self.saved_video.is_some() {
                if let Some(player) = self.video_preview.as_mut() {
                    ui.add_space(5.0);
                    player.timeline_ui(ui);
                    ui.horizontal_wrapped(|ui| {
                        let button = if player.playing() { "⏸ Pause" } else { "▶ Play" };
                        if ui.button(button).clicked() {
                            if player.playing() { player.pause(); } else { player.play(); }
                        }
                        if ui.button("↺ Replay").clicked() { player.replay(); }
                        ui.monospace(match player.duration() {
                            Some(duration) => format!("{} / {}",
                                playback::format_time(player.position()), playback::format_time(duration)),
                            None => format!("{} / --:--", playback::format_time(player.position())),
                        });
                        ui.label("Drag to seek • ←/→ 5s • Pause to edit • Preview muted");
                    });
                    if let Some(error) = player.error() {
                        ui.colored_label(Color32::LIGHT_RED, error);
                    }
                }
            }
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| { ui.label(&self.status); });
        let ended = self.recorder.as_mut().and_then(|r|
            r.child.try_wait().ok().flatten().map(|status| (status, r.temp_path.clone())));
        if let Some((status, path)) = ended {
            self.recorder.take();
            let valid = fs::metadata(&path).map(|m| m.len() > 1024).unwrap_or(false);
            if status.success() && valid {
                self.prepare_recording_for_review(path, ctx);
            } else {
                self.status = if valid {
                    format!("Recorder exited unexpectedly ({}). Video preserved at {}", status, path.display())
                } else {
                    format!("Recorder stopped unexpectedly ({}). Check FFmpeg and capture permissions.", status)
                };
            }
        }
        if self.recorder.is_some() || self.export_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// FFmpeg composites a transparent PNG overlay over every frame of an existing
/// recording. A temporary output and a distinct destination protect originals.
fn render_annotated_video(ffmpeg: &Path, source: &Path, dest: &Path, overlay: &RgbaImage) -> Result<PathBuf, String> {
    let temp = std::env::temp_dir().join("NeuroSnip").join("VideoEdits")
        .join(chrono_free_stamp());
    fs::create_dir_all(&temp).map_err(|e| format!("Cannot make render folder: {e}"))?;
    let overlay_path = temp.join("annotations.png");
    let temp_output = temp.join("annotated.mp4");
    overlay.save(&overlay_path).map_err(|e| format!("Cannot save overlay image: {e}"))?;

    let result = hidden_ffmpeg(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i").arg(source)
        .args(["-loop", "1", "-framerate", "30", "-i"]).arg(&overlay_path)
        .args(["-filter_complex", "[0:v][1:v]overlay=0:0:format=auto:shortest=1[v]",
            "-map", "[v]", "-map", "0:a?", "-c:v", "libx264", "-preset", "veryfast",
            "-crf", "23", "-pix_fmt", "yuv420p", "-c:a", "copy",
            "-movflags", "+faststart", "-shortest"])
        .arg(&temp_output)
        .output().map_err(|e| format!("Cannot launch FFmpeg: {e}"))?;
    if !result.status.success() {
        return Err(format!("FFmpeg reported: {}", String::from_utf8_lossy(&result.stderr)));
    }
    if fs::metadata(&temp_output).map(|m| m.len() < 1024).unwrap_or(true) {
        return Err("FFmpeg did not create a valid annotated MP4".into());
    }
    fs::copy(&temp_output, dest).map_err(|e| format!("Cannot save annotated MP4: {e}"))?;
    let _ = fs::remove_dir_all(&temp);
    Ok(dest.to_path_buf())
}

/// Resolve equivalent existing paths without accidentally treating two
/// nonexistent paths as equal (None == None).
fn recording_paths_match(a: &Path, b: &Path) -> bool {
    a == b || fs::canonicalize(a).ok().zip(fs::canonicalize(b).ok())
        .is_some_and(|(one, two)| one == two)
}

#[cfg(test)]
mod save_tests {
    use super::recording_paths_match;
    use std::path::Path;

    #[test]
    fn equal_paths_are_rejected() {
        assert!(recording_paths_match(Path::new("video.mp4"), Path::new("video.mp4")));
    }
    #[test]
    fn nonexistent_different_paths_are_not_equal() {
        assert!(!recording_paths_match(Path::new("not-a-real-path-1.mp4"), Path::new("not-a-real-path-2.mp4")));
    }
}

fn even_floor(n: u32) -> u32 { n & !1 }
fn chrono_free_stamp() -> String { use std::time::{SystemTime, UNIX_EPOCH}; SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_string() }

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 680.0]).with_min_inner_size([680.0, 480.0]), ..Default::default() };
    eframe::run_native("NeuroSnip v1.8", options, Box::new(|cc| {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.window_fill = Color32::from_rgb(28, 28, 30);
        style.visuals.panel_fill = Color32::from_rgb(24, 24, 26);
        cc.egui_ctx.set_style(style);
        Ok(Box::new(NeuroSnip::default()))
    }))
}
