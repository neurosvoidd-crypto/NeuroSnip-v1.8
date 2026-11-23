//! Embedded, silent MP4 preview for NeuroSnip's annotation canvas.
//!
//! Uses the already-bundled FFmpeg executable; no external video player is
//! required. Frames are decoded at a capped preview resolution on a worker
//! thread so the egui event loop stays responsive. Output MP4 audio is untouched.

use eframe::egui::{self, TextureOptions};
use crate::ffmpeg_process::hidden_ffmpeg;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, SyncSender}, Arc},
    thread,
    time::{Duration, Instant},
};

const PREVIEW_FPS: u32 = 15;
const MAX_PREVIEW_WIDTH: u32 = 960;
const MAX_PREVIEW_HEIGHT: u32 = 540;

enum DecoderEvent {
    Frame(Vec<u8>),
    Error(String),
}

pub struct VideoPreview {
    ffmpeg: PathBuf,
    video: PathBuf,
    frame_size: [u32; 2],
    duration: Option<f64>,
    position: f64,
    scrub_position: Option<f64>,
    play_start: Option<(f64, Instant)>,
    playing: bool,
    stream: Option<Receiver<DecoderEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    texture: Option<egui::TextureHandle>,
    error: Option<String>,
}

impl VideoPreview {
    pub fn open(ffmpeg: PathBuf, video: PathBuf, size: [u32; 2]) -> Result<Self, String> {
        if !video.is_file() { return Err("The saved recording could not be found".into()); }
        if size[0] == 0 || size[1] == 0 { return Err("The recording has invalid video dimensions".into()); }
        let frame_size = preview_dimensions(size);
        let duration = probe_duration(&ffmpeg, &video);
        Ok(Self {
            ffmpeg, video, frame_size, duration,
            position: 0.0, scrub_position: None, play_start: None, playing: false,
            stream: None, cancel: None, texture: None, error: None,
        })
    }

    pub fn playing(&self) -> bool { self.playing }
    pub fn position(&self) -> f64 { self.scrub_position.unwrap_or(self.position) }
    pub fn duration(&self) -> Option<f64> { self.duration }
    pub fn texture_id(&self) -> Option<egui::TextureId> { self.texture.as_ref().map(|t| t.id()) }
    pub fn error(&self) -> Option<&str> { self.error.as_deref() }

    pub fn play(&mut self) {
        if self.playing { return; }
        if let Some(target) = self.scrub_position.take() { self.position = target; }
        self.stop_worker();
        if let Some(duration) = self.duration {
            if self.position >= (duration - 0.07).max(0.0) { self.position = 0.0; }
        }
        self.error = None;
        let (tx, rx) = mpsc::sync_channel(2);
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let ffmpeg = self.ffmpeg.clone();
        let video = self.video.clone();
        let size = self.frame_size;
        let start = self.position;
        thread::spawn(move || decode_stream(ffmpeg, video, size, start, worker_flag, tx));
        self.cancel = Some(flag);
        self.stream = Some(rx);
        self.play_start = Some((start, Instant::now()));
        self.playing = true;
    }

    pub fn pause(&mut self) {
        if let Some((offset, time)) = self.play_start.take() {
            self.position = self.clamp_time(offset + time.elapsed().as_secs_f64());
        }
        self.playing = false;
        self.stop_worker();
    }

    pub fn replay(&mut self) {
        self.pause();
        self.scrub_position = None;
        self.position = 0.0;
        self.play();
    }

    /// Seeking is asynchronous. The current frame stays visible until the
    /// requested frame has decoded, including while the video is paused.
    pub fn seek(&mut self, seconds: f64) {
        self.pause();
        self.scrub_position = None;
        self.position = self.clamp_time(seconds);
        self.error = None;
        let (tx, rx) = mpsc::sync_channel(1);
        let ffmpeg = self.ffmpeg.clone();
        let video = self.video.clone();
        let size = self.frame_size;
        // At the exact EOF FFmpeg has no decodable image; seek to the
        // nearest preceding frame while retaining the user's timeline value.
        let at = self.duration.map(|end| self.position.min((end - 0.10).max(0.0)))
            .unwrap_or(self.position);
        thread::spawn(move || {
            match extract_frame(&ffmpeg, &video, size, at) {
                Ok(frame) => { let _ = tx.send(DecoderEvent::Frame(frame)); }
                Err(error) => { let _ = tx.send(DecoderEvent::Error(error)); }
            }
        });
        self.stream = Some(rx);
    }

    /// Pause as soon as the playhead is grabbed. Only decode the final requested
    /// frame when the drag ends: this avoids spawning dozens of FFmpeg processes
    /// while moving the pointer across the timeline.
    fn begin_scrub(&mut self) {
        self.pause();
        self.scrub_position = Some(self.position);
    }

    fn scrub_to(&mut self, seconds: f64) {
        self.scrub_position = Some(self.clamp_time(seconds));
    }

    fn finish_scrub(&mut self) {
        if let Some(target) = self.scrub_position.take() { self.seek(target); }
    }

    /// An interactive YouTube-style seek bar. Click anywhere to jump, or drag
    /// the playhead to a time; both actions leave playback paused for editing.
    /// Left/Right keys seek five seconds when the bar has keyboard focus.
    pub fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let Some(duration) = self.duration.filter(|d| *d > 0.0 && d.is_finite()) else {
            ui.label("Timeline unavailable: could not read the recording duration");
            return;
        };

        let size = egui::vec2(ui.available_width().max(80.0), 39.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 9.0, rect.bottom() - 14.0),
            egui::pos2(rect.right() - 9.0, rect.bottom() - 8.0),
        );
        let at_x = |x: f32| seek_time_at_x(x, track.left(), track.right(), duration);

        if response.drag_started() {
            self.begin_scrub();
            if let Some(pointer) = response.interact_pointer_pos() {
                self.scrub_to(at_x(pointer.x));
            }
        }
        if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                self.scrub_to(at_x(pointer.x));
            }
        }
        if response.drag_stopped() {
            if let Some(pointer) = response.interact_pointer_pos() {
                self.scrub_to(at_x(pointer.x));
            }
            self.finish_scrub();
        }
        if response.clicked() {
            if let Some(pointer) = response.interact_pointer_pos() {
                self.seek(at_x(pointer.x));
            }
        }
        if response.has_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.scrub_position = None;
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                self.seek(self.position() - 5.0);
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                self.seek(self.position() + 5.0);
            } else if ui.input(|i| i.key_pressed(egui::Key::Home)) {
                self.seek(0.0);
            } else if ui.input(|i| i.key_pressed(egui::Key::End)) {
                self.seek(duration);
            }
        }

        let played = (self.position() / duration).clamp(0.0, 1.0) as f32;
        let playhead_x = track.left() + track.width() * played;
        let hover_x = response.hover_pos().map(|pointer|
            pointer.x.clamp(track.left(), track.right()));
        if hover_x.is_some() {
            ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
        }
        let painter = ui.painter();
        painter.rect_filled(track, 3.0, egui::Color32::from_rgb(66, 69, 78));
        painter.rect_filled(egui::Rect::from_min_max(track.min,
            egui::pos2(playhead_x, track.bottom())), 3.0,
            egui::Color32::from_rgb(65, 170, 245));
        painter.circle_filled(egui::pos2(playhead_x, track.center().y),
            if response.hovered() || self.scrub_position.is_some() { 7.0 } else { 5.5 },
            egui::Color32::from_rgb(220, 240, 255));

        if let Some(hover_x) = hover_x {
            let label = format_time(at_x(hover_x));
            painter.text(egui::pos2(hover_x, rect.top() + 6.0),
                egui::Align2::CENTER_TOP, label,
                egui::FontId::monospace(12.0), egui::Color32::LIGHT_GRAY);
        }
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        if let Some((offset, start)) = self.play_start {
            self.position = self.clamp_time(offset + start.elapsed().as_secs_f64());
        }
        let mut newest_frame = None;
        let mut disconnected = false;
        if let Some(rx) = &self.stream {
            loop {
                match rx.try_recv() {
                    Ok(DecoderEvent::Frame(pixels)) => newest_frame = Some(pixels),
                    Ok(DecoderEvent::Error(msg)) => self.error = Some(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => { disconnected = true; break; }
                }
            }
        }
        if let Some(pixels) = newest_frame {
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [self.frame_size[0] as usize, self.frame_size[1] as usize], &pixels);
            if let Some(texture) = self.texture.as_mut() {
                texture.set(color, TextureOptions::LINEAR);
            } else {
                self.texture = Some(ctx.load_texture(
                    "video-playback-preview", color, TextureOptions::LINEAR));
            }
            ctx.request_repaint();
        }
        if disconnected {
            self.stream = None;
            if self.playing {
                self.playing = false;
                self.play_start = None;
                // The decoder reached the end of the file. Keep the last frame.
                if let Some(duration) = self.duration { self.position = duration; }
                self.stop_worker();
            }
        }
        if self.playing || self.stream.is_some() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }

    fn clamp_time(&self, time: f64) -> f64 {
        if !time.is_finite() { return 0.0; }
        match self.duration {
            Some(end) => time.clamp(0.0, end.max(0.0)),
            None => time.max(0.0),
        }
    }

    fn stop_worker(&mut self) {
        if let Some(flag) = self.cancel.take() { flag.store(true, Ordering::Relaxed); }
        self.stream = None;
    }
}

impl Drop for VideoPreview {
    fn drop(&mut self) { self.stop_worker(); }
}

fn preview_dimensions([width, height]: [u32; 2]) -> [u32; 2] {
    let scale = (MAX_PREVIEW_WIDTH as f64 / width.max(1) as f64)
        .min(MAX_PREVIEW_HEIGHT as f64 / height.max(1) as f64)
        .min(1.0);
    [((width as f64 * scale).round() as u32).max(1),
     ((height as f64 * scale).round() as u32).max(1)]
}

fn probe_duration(ffmpeg: &Path, video: &Path) -> Option<f64> {
    // FFmpeg already ships inside NeuroSnip. It prints MP4 metadata on stderr
    // even when no output file is specified, so ffprobe isn't required.
    let output = hidden_ffmpeg(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"]).arg(video)
        .stdin(Stdio::null()).output().ok()?;
    parse_duration(&String::from_utf8_lossy(&output.stderr))
}

fn parse_duration(log: &str) -> Option<f64> {
    let stamp = log.split("Duration:").nth(1)?.split(',').next()?.trim();
    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !hours.is_finite() || !minutes.is_finite() ||
        !seconds.is_finite() || hours < 0.0 || minutes < 0.0 || seconds < 0.0 {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn decode_stream(ffmpeg: PathBuf, video: PathBuf, size: [u32; 2],
    start: f64, cancel: Arc<AtomicBool>, tx: SyncSender<DecoderEvent>) {
    let scale = format!("fps={PREVIEW_FPS},scale={}:{}:flags=fast_bilinear", size[0], size[1]);
    let start_arg = format!("{start:.3}");
    let mut child = match hidden_ffmpeg(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &start_arg, "-i"])
        .arg(&video)
        .args(["-an", "-sn", "-dn", "-vf", &scale,
            "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = tx.send(DecoderEvent::Error(format!("Cannot start video decoder: {error}")));
            return;
        }
    };
    let bytes_per_frame = size[0] as usize * size[1] as usize * 4;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => { let _ = child.kill(); let _ = child.wait(); return; }
    };
    let begin = Instant::now();
    let mut count = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) { break; }
        let mut bytes = vec![0u8; bytes_per_frame];
        match stdout.read_exact(&mut bytes) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        }
        // Rate-limit decoding to 15 FPS. This prevents a short recording from
        // decoding at full speed while the user is trying to watch it.
        let due = Duration::from_secs_f64(count as f64 / PREVIEW_FPS as f64);
        while begin.elapsed() < due {
            if cancel.load(Ordering::Relaxed) { break; }
            thread::sleep(Duration::from_millis(5));
        }
        if cancel.load(Ordering::Relaxed) || tx.send(DecoderEvent::Frame(bytes)).is_err() { break; }
        count += 1;
    }
    if cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
    }
    drop(stdout);
    if let Ok(status) = child.wait() {
        if !status.success() && !cancel.load(Ordering::Relaxed) {
            let _ = tx.try_send(DecoderEvent::Error(
                "FFmpeg could not decode this recording".into()));
        }
    }
}

fn extract_frame(ffmpeg: &Path, video: &Path, size: [u32; 2], seconds: f64)
    -> Result<Vec<u8>, String> {
    let scale = format!("scale={}:{}:flags=fast_bilinear", size[0], size[1]);
    let at = format!("{seconds:.3}");
    let output = hidden_ffmpeg(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &at, "-i"])
        .arg(video)
        .args(["-an", "-sn", "-dn", "-frames:v", "1", "-vf", &scale,
            "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null()).output()
        .map_err(|e| format!("Cannot seek video: {e}"))?;
    let expected = size[0] as usize * size[1] as usize * 4;
    if !output.status.success() || output.stdout.len() != expected {
        return Err(format!("Could not read video frame at {seconds:.2}s: {}",
            String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

/// Pure mapping for mouse movement, including clicks outside the bar ends.
fn seek_time_at_x(x: f32, left: f32, right: f32, duration: f64) -> f64 {
    if duration <= 0.0 || !duration.is_finite() || (right - left) <= 0.0 {
        return 0.0;
    }
    (f64::from(((x - left) / (right - left)).clamp(0.0, 1.0))) * duration
}

pub fn format_time(seconds: f64) -> String {
    let whole = seconds.max(0.0) as u64;
    if whole >= 3600 {
        format!("{}:{:02}:{:02}", whole / 3600, (whole / 60) % 60, whole % 60)
    } else {
        format!("{:02}:{:02}", whole / 60, whole % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_time, parse_duration, preview_dimensions, seek_time_at_x};

    #[test]
    fn duration_from_ffmpeg_metadata() {
        assert_eq!(parse_duration("  Duration: 00:01:23.45, start: 0.0"), Some(83.45));
        assert_eq!(parse_duration("Duration: N/A, start: 0"), None);
    }

    #[test]
    fn preview_respects_aspect_ratio() {
        assert_eq!(preview_dimensions([1920, 1080]), [960, 540]);
        assert_eq!(preview_dimensions([1080, 1920]), [304, 540]);
        assert_eq!(preview_dimensions([320, 240]), [320, 240]);
    }

    #[test]
    fn time_label() {
        assert_eq!(format_time(65.2), "01:05");
        assert_eq!(format_time(3661.2), "1:01:01");
    }

    #[test]
    fn seek_bar_maps_mouse_to_time_and_clamps_edges() {
        let duration = 120.0;
        assert_eq!(seek_time_at_x(10.0, 10.0, 210.0, duration), 0.0);
        assert_eq!(seek_time_at_x(110.0, 10.0, 210.0, duration), 60.0);
        assert_eq!(seek_time_at_x(210.0, 10.0, 210.0, duration), 120.0);
        assert_eq!(seek_time_at_x(-200.0, 10.0, 210.0, duration), 0.0);
        assert_eq!(seek_time_at_x(800.0, 10.0, 210.0, duration), 120.0);
        assert_eq!(seek_time_at_x(15.0, 10.0, 10.0, duration), 0.0);
    }
}
