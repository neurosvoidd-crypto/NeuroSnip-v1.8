//! Non-destructive editing shared by screenshots and completed recordings.
//! All positions are normalized to the selected image/video frame.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use image::{Rgba, RgbaImage};
use std::{fs, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool { Pen, Highlighter, Eraser, Text }

#[derive(Clone)]
struct BrushStroke {
    points: Vec<[f32; 2]>,
    color: [u8; 4],
    thickness: f32, // Output pixels (not preview pixels).
}

#[derive(Clone)]
struct TextMark {
    at: [f32; 2],
    content: String,
    color: [u8; 4],
    size: f32, // Output pixels (not preview pixels).
}

#[derive(Clone)]
enum Mark {
    Brush(BrushStroke),
    Text(TextMark),
}

/// Dragging stores the start coordinates, so movement does not drift if the
/// pointer goes outside the preview. A single undo checkpoint covers a drag.
#[derive(Clone, Copy)]
enum TextDrag {
    Move {
        index: usize,
        pointer_start: [f32; 2],
        mark_start: [f32; 2],
        checkpointed: bool,
    },
    Resize {
        index: usize,
        pointer_start: [f32; 2],
        start_size: f32,
        start_width: f32,
        start_height: f32,
        checkpointed: bool,
    },
}

/// Direct editing takes place on top of the preview, without a separate tool.
struct InlineTextEdit {
    index: usize,
    draft: String,
    needs_focus: bool,
}

/// One eraser gesture = one undo step, even if it removes many marks.
/// Coordinates are normalized to the image, not preview/window pixels.
struct EraserDrag {
    previous: [f32; 2],
    checkpointed: bool,
}

pub struct Editor {
    pub tool: Tool,
    pub color: Color32,
    pub brush_size: f32,
    /// Diameter of the object eraser in output pixels.
    pub eraser_size: f32,
    pub text_size: f32,
    pub text_input: String,
    /// Values shown in the properties panel for the selected placed label.
    pub edit_text_input: String,
    pub edit_text_color: Color32,
    pub edit_text_size: f32,
    selected_text: Option<usize>,
    text_drag: Option<TextDrag>,
    inline_edit: Option<InlineTextEdit>,
    eraser_drag: Option<EraserDrag>,
    marks: Vec<Mark>,
    active_points: Vec<[f32; 2]>,
    undo_stack: Vec<Vec<Mark>>,
    redo_stack: Vec<Vec<Mark>>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            tool: Tool::Pen,
            color: Color32::from_rgb(46, 154, 255),
            brush_size: 5.0,
            eraser_size: 30.0,
            text_size: 48.0,
            text_input: String::new(),
            edit_text_input: String::new(),
            edit_text_color: Color32::from_rgb(46, 154, 255),
            edit_text_size: 48.0,
            selected_text: None,
            text_drag: None,
            inline_edit: None,
            eraser_drag: None,
            marks: Vec::new(),
            active_points: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }
}

impl Editor {
    pub fn reset(&mut self) {
        self.marks.clear();
        self.active_points.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.eraser_drag = None;
        self.deselect_text();
    }

    pub fn has_edits(&self) -> bool { !self.marks.is_empty() }
    pub fn can_undo(&self) -> bool { !self.undo_stack.is_empty() }
    pub fn can_redo(&self) -> bool { !self.redo_stack.is_empty() }
    pub fn has_selected_text(&self) -> bool {
        self.selected_text.and_then(|i| self.marks.get(i))
            .is_some_and(|mark| matches!(mark, Mark::Text(_)))
    }

    /// Keep text selection and pending form edits outside the rasterized marks.
    pub fn deselect_text(&mut self) {
        self.selected_text = None;
        self.text_drag = None;
        self.inline_edit = None;
        self.edit_text_input.clear();
    }

    fn select_text(&mut self, index: usize) {
        if let Some(Mark::Text(mark)) = self.marks.get(index) {
            self.selected_text = Some(index);
            self.edit_text_input = mark.content.clone();
            self.edit_text_color = Color32::from_rgba_unmultiplied(
                mark.color[0], mark.color[1], mark.color[2], mark.color[3]);
            self.edit_text_size = mark.size;
        }
    }

    /// Approximate the displayed Segoe/egui label bounds for grabbing and
    /// highlighting. Multiline labels are supported in the properties editor.
    fn text_bounds(mark: &TextMark, image_size: [u32; 2]) -> [f32; 4] {
        let max_chars = mark.content.split('\n').map(|line| line.chars().count())
            .max().unwrap_or(0).max(1);
        let line_count = mark.content.split('\n').count().max(1);
        let width = (max_chars as f32 * mark.size * 0.60).max(mark.size * 0.5);
        let height = line_count as f32 * mark.size * 1.25;
        [mark.at[0] * image_size[0] as f32,
         mark.at[1] * image_size[1] as f32, width, height]
    }

    fn text_at(&self, pos: [f32; 2], image_size: [u32; 2]) -> Option<usize> {
        let x = pos[0] * image_size[0] as f32;
        let y = pos[1] * image_size[1] as f32;
        // Use a small mouse-friendly hit margin, in output pixels.
        let margin = 7.0;
        self.marks.iter().enumerate().rev().find_map(|(index, mark)| {
            let Mark::Text(label) = mark else { return None; };
            let [left, top, width, height] = Self::text_bounds(label, image_size);
            (x >= left - margin && x <= left + width + margin &&
             y >= top - margin && y <= top + height + margin).then_some(index)
        })
    }

    pub fn hovering_text(&self, pos: [f32; 2], image_size: [u32; 2]) -> bool {
        self.tool != Tool::Eraser && self.text_at(pos, image_size).is_some()
    }

    /// The bottom-right handle is measured in *preview pixels* so it stays
    /// easy to grab even when a large screenshot is scaled down for display.
    pub fn hovering_resize_handle(&self, pos: [f32; 2], image_size: [u32; 2], preview_scale: f32) -> bool {
        if self.tool == Tool::Eraser || self.inline_edit.is_some() { return false; }
        let Some(index) = self.selected_text else { return false; };
        let Some(Mark::Text(mark)) = self.marks.get(index) else { return false; };
        let [left, top, width, height] = Self::text_bounds(mark, image_size);
        let mouse = output_point(pos, image_size);
        (mouse[0] - (left + width)).hypot(mouse[1] - (top + height))
            * preview_scale.max(0.001) <= 12.0
    }

    pub fn editing_inline(&self) -> bool { self.inline_edit.is_some() }

    /// Double-click an existing label to edit the words in place.
    pub fn start_inline_edit_at(&mut self, pos: [f32; 2], image_size: [u32; 2]) -> bool {
        if self.tool == Tool::Eraser { return false; }
        let Some(index) = self.text_at(pos, image_size) else { return false; };
        self.select_text(index);
        let Some(Mark::Text(label)) = self.marks.get(index) else { return false; };
        self.text_drag = None;
        self.inline_edit = Some(InlineTextEdit {
            index, draft: label.content.clone(), needs_focus: true,
        });
        true
    }

    /// Commit one Undo checkpoint for the whole text rewrite (or cancel it).
    fn finish_inline_edit(&mut self, save: bool) -> Result<(), String> {
        let Some(edit) = self.inline_edit.take() else { return Ok(()); };
        if !save { return Ok(()); }
        if edit.draft.trim().is_empty() {
            return Err("Text cannot be blank; use Delete text to remove it".into());
        }
        let Some(Mark::Text(label)) = self.marks.get(edit.index) else {
            return Err("The text label was removed during editing".into());
        };
        if label.content != edit.draft {
            self.checkpoint();
            if let Some(Mark::Text(label)) = self.marks.get_mut(edit.index) {
                label.content = edit.draft;
            }
        }
        self.select_text(edit.index);
        Ok(())
    }

    /// Draw an editable textbox over the label, in the exact preview region.
    /// Enter or clicking away commits; Esc cancels; Shift+Enter adds a line.
    pub fn inline_text_ui(&mut self, ui: &mut egui::Ui, preview: Rect, image_size: [u32; 2]) -> Option<Result<(), String>> {
        let edit = self.inline_edit.as_mut()?;
        let Some(Mark::Text(mark)) = self.marks.get(edit.index) else {
            self.inline_edit = None;
            return Some(Err("Text label no longer exists".into()));
        };
        let [left, top, width, height] = Self::text_bounds(mark, image_size);
        let sx = preview.width() / image_size[0].max(1) as f32;
        let sy = preview.height() / image_size[1].max(1) as f32;
        let origin = preview.min + Vec2::new(left * sx, top * sy);
        let available_width = (preview.max.x - origin.x).max(32.0);
        let available_height = (preview.max.y - origin.y).max(22.0);
        let edit_rect = Rect::from_min_size(origin,
            Vec2::new((width * sx + 44.0).max(150.0).min(available_width),
                (height * sy + 20.0).max(34.0).min(available_height)));
        let response = ui.put(edit_rect,
            egui::TextEdit::multiline(&mut edit.draft)
                .font(FontId::proportional((mark.size * sx).max(10.0)))
                .desired_width(edit_rect.width())
                .desired_rows(1));
        if edit.needs_focus {
            response.request_focus();
            edit.needs_focus = false;
        }
        let (confirm, cancel) = ui.input(|i| (
            i.key_pressed(egui::Key::Enter) && !i.modifiers.shift,
            i.key_pressed(egui::Key::Escape),
        ));
        if cancel { return Some(self.finish_inline_edit(false)); }
        if confirm || response.lost_focus() { return Some(self.finish_inline_edit(true)); }
        None
    }

    pub fn apply_selected_text(&mut self) -> Result<(), String> {
        let Some(index) = self.selected_text else { return Err("Select an existing text label first".into()); };
        if self.edit_text_input.trim().is_empty() {
            return Err("Text cannot be empty; use Delete text to remove the label".into());
        }
        let new_content = self.edit_text_input.clone();
        let new_color = self.edit_text_color.to_array();
        let new_size = self.edit_text_size;
        let Some(Mark::Text(current)) = self.marks.get(index) else {
            self.deselect_text();
            return Err("The selected text no longer exists".into());
        };
        if current.content == new_content && current.color == new_color &&
            (current.size - new_size).abs() < f32::EPSILON { return Ok(()); }
        self.checkpoint();
        if let Some(Mark::Text(mark)) = self.marks.get_mut(index) {
            mark.content = new_content;
            mark.color = new_color;
            mark.size = new_size;
        }
        Ok(())
    }

    pub fn delete_selected_text(&mut self) {
        if let Some(index) = self.selected_text {
            if matches!(self.marks.get(index), Some(Mark::Text(_))) {
                self.checkpoint();
                self.marks.remove(index);
            }
        }
        self.deselect_text();
    }

    fn checkpoint(&mut self) {
        if self.undo_stack.len() >= 40 { self.undo_stack.remove(0); }
        self.undo_stack.push(self.marks.clone());
        self.redo_stack.clear();
    }

    pub fn undo(&mut self) {
        self.eraser_drag = None;
        if let Some(previous) = self.undo_stack.pop() {
            self.redo_stack.push(std::mem::replace(&mut self.marks, previous));
            self.deselect_text();
        }
    }

    pub fn redo(&mut self) {
        self.eraser_drag = None;
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(std::mem::replace(&mut self.marks, next));
            self.deselect_text();
        }
    }

    pub fn clear(&mut self) {
        if !self.marks.is_empty() {
            self.checkpoint();
            self.marks.clear();
        }
        self.deselect_text();
        self.active_points.clear();
        self.eraser_drag = None;
    }

    pub fn begin_drag(&mut self, pos: [f32; 2], image_size: [u32; 2], preview_scale: f32) {
        if self.inline_edit.is_some() { return; }
        self.active_points.clear();
        self.text_drag = None;
        self.eraser_drag = None;
        if self.tool == Tool::Eraser {
            self.deselect_text();
            self.eraser_drag = Some(EraserDrag { previous: pos, checkpointed: false });
            self.erase_along(pos, pos, image_size);
            return;
        }
        // Direct text manipulation works while Pen, Marker, or Add text is
        // selected: the user never needs to switch to a Move/Edit tool.
        if self.hovering_resize_handle(pos, image_size, preview_scale) {
            let index = self.selected_text.unwrap();
            if let Some(Mark::Text(mark)) = self.marks.get(index) {
                let [_, _, width, height] = Self::text_bounds(mark, image_size);
                self.text_drag = Some(TextDrag::Resize {
                    index,
                    pointer_start: pos,
                    start_size: mark.size,
                    start_width: width,
                    start_height: height,
                    checkpointed: false,
                });
            }
            return;
        }
        if let Some(index) = self.text_at(pos, image_size) {
            self.select_text(index);
            if let Some(Mark::Text(mark)) = self.marks.get(index) {
                self.text_drag = Some(TextDrag::Move {
                    index,
                    pointer_start: pos,
                    mark_start: mark.at,
                    checkpointed: false,
                });
            }
            return;
        }
        if matches!(self.tool, Tool::Pen | Tool::Highlighter) {
            self.deselect_text();
            self.active_points.push(pos);
        }
    }

    pub fn continue_drag(&mut self, pos: [f32; 2], image_size: [u32; 2]) {
        if let Some(previous) = self.eraser_drag.as_ref().map(|drag| drag.previous) {
            self.erase_along(previous, pos, image_size);
            if let Some(drag) = &mut self.eraser_drag { drag.previous = pos; }
            return;
        }
        // Copy drag parameters before checkpoint() mutably borrows the editor.
        match self.text_drag {
            Some(TextDrag::Move { index, pointer_start, mark_start, checkpointed }) => {
                let Some(Mark::Text(mark)) = self.marks.get(index) else { return; };
                let [_, _, width, height] = Self::text_bounds(mark, image_size);
                let max_x = (1.0 - width / image_size[0].max(1) as f32).max(0.0);
                let max_y = (1.0 - height / image_size[1].max(1) as f32).max(0.0);
                let x = (mark_start[0] + pos[0] - pointer_start[0]).clamp(0.0, max_x);
                let y = (mark_start[1] + pos[1] - pointer_start[1]).clamp(0.0, max_y);
                if (x - mark.at[0]).hypot(y - mark.at[1]) > 0.00001 {
                    if !checkpointed {
                        self.checkpoint();
                        if let Some(TextDrag::Move { checkpointed, .. }) = &mut self.text_drag {
                            *checkpointed = true;
                        }
                    }
                    if let Some(Mark::Text(mark)) = self.marks.get_mut(index) { mark.at = [x, y]; }
                }
            }
            Some(TextDrag::Resize { index, pointer_start, start_size, start_width, start_height, checkpointed }) => {
                let Some(Mark::Text(mark)) = self.marks.get(index) else { return; };
                let dx = (pos[0] - pointer_start[0]) * image_size[0] as f32;
                let dy = (pos[1] - pointer_start[1]) * image_size[1] as f32;
                // Use the dominant axis so dragging only left/right OR up/down
                // resizes the text. Keep the label's top-left corner fixed.
                let ratio = if (dx / start_width.max(1.0)).abs() >= (dy / start_height.max(1.0)).abs() {
                    1.0 + dx / start_width.max(1.0)
                } else { 1.0 + dy / start_height.max(1.0) };
                let new_size = (start_size * ratio).clamp(12.0, 256.0);
                if (new_size - mark.size).abs() > 0.05 {
                    if !checkpointed {
                        self.checkpoint();
                        if let Some(TextDrag::Resize { checkpointed, .. }) = &mut self.text_drag {
                            *checkpointed = true;
                        }
                    }
                    if let Some(Mark::Text(mark)) = self.marks.get_mut(index) { mark.size = new_size; }
                    self.edit_text_size = new_size;
                }
            }
            None => {
                if matches!(self.tool, Tool::Pen | Tool::Highlighter) && !self.active_points.is_empty() {
                    if self.active_points.last().is_some_and(|last|
                        (last[0] - pos[0]).hypot(last[1] - pos[1]) > 0.00005)
                    {
                        self.active_points.push(pos);
                    }
                }
            }
        }
    }

    pub fn end_drag(&mut self) {
        if self.eraser_drag.take().is_some() { return; }
        if self.text_drag.take().is_some() { return; }
        if !matches!(self.tool, Tool::Pen | Tool::Highlighter) { self.active_points.clear(); return; }
        if self.active_points.is_empty() { return; }
        let points = std::mem::take(&mut self.active_points);
        let mut color = self.color.to_array();
        let thickness = if self.tool == Tool::Highlighter {
            color[3] = 100;
            self.brush_size * 3.0
        } else { self.brush_size };
        self.checkpoint();
        self.marks.push(Mark::Brush(BrushStroke { points, color, thickness }));
    }

    pub fn click(&mut self, pos: [f32; 2], image_size: [u32; 2]) -> Result<(), String> {
        if self.inline_edit.is_some() { return Ok(()); }
        if self.tool == Tool::Eraser {
            self.erase_along(pos, pos, image_size);
            return Ok(());
        }
        // Selecting text always takes priority over drawing or adding labels.
        if let Some(index) = self.text_at(pos, image_size) {
            self.select_text(index);
            return Ok(());
        }
        self.deselect_text();
        if self.tool != Tool::Text { return Ok(()); }
        let text = self.text_input.trim();
        if text.is_empty() { return Err("Enter text in the toolbar before placing it".into()); }
        let mark = TextMark {
            at: pos,
            content: text.to_owned(),
            color: self.color.to_array(),
            size: self.text_size,
        };
        self.checkpoint();
        self.marks.push(Mark::Text(mark));
        self.select_text(self.marks.len() - 1);
        Ok(())
    }

    /// Remove any brush strokes or editable text touched by the eraser path.
    /// A whole annotation is removed, rather than altering the captured media.
    /// Each click is one undo step; an arbitrarily long drag is also one step.
    fn erase_along(&mut self, from: [f32; 2], to: [f32; 2], image_size: [u32; 2]) -> usize {
        let radius = (self.eraser_size * 0.5).max(0.5);
        let p0 = output_point(from, image_size);
        let p1 = output_point(to, image_size);
        let hits: Vec<usize> = self.marks.iter().enumerate()
            .filter_map(|(index, mark)| {
                eraser_hits_mark(mark, p0, p1, image_size, radius).then_some(index)
            })
            .collect();
        if hits.is_empty() { return 0; }

        // Do not create a new undo snapshot on every pointer-move event.
        let needs_checkpoint = self.eraser_drag.as_ref().map_or(true, |drag| !drag.checkpointed);
        if needs_checkpoint {
            self.checkpoint();
            if let Some(drag) = &mut self.eraser_drag { drag.checkpointed = true; }
        }
        // Removing in descending order preserves the indexes gathered above.
        for &index in hits.iter().rev() { self.marks.remove(index); }
        self.deselect_text();
        hits.len()
    }

    pub fn paint_preview(&self, ui: &egui::Ui, rect: Rect, output_width: u32, output_height: u32) {
        let scale_x = rect.width() / output_width.max(1) as f32;
        let scale_y = rect.height() / output_height.max(1) as f32;
        let scale = scale_x.min(scale_y);
        for (index, mark) in self.marks.iter().enumerate() {
            match mark {
                Mark::Brush(stroke) => {
                    let color = Color32::from_rgba_unmultiplied(
                        stroke.color[0], stroke.color[1], stroke.color[2], stroke.color[3]);
                    let points: Vec<Pos2> = stroke.points.iter().map(|p| Pos2::new(
                        rect.min.x + p[0] * rect.width(), rect.min.y + p[1] * rect.height())).collect();
                    if points.len() == 1 {
                        ui.painter().circle_filled(points[0], (stroke.thickness * scale * 0.5).max(1.0), color);
                    } else if points.len() > 1 {
                        ui.painter().add(egui::Shape::line(points, Stroke::new((stroke.thickness * scale).max(1.0), color)));
                    }
                }
                Mark::Text(t) => {
                    let pos = Pos2::new(rect.min.x + t.at[0] * rect.width(), rect.min.y + t.at[1] * rect.height());
                    if self.inline_edit.as_ref().map(|edit| edit.index) != Some(index) {
                        ui.painter().text(pos, Align2::LEFT_TOP, &t.content,
                            FontId::proportional((t.size * scale).max(7.0)),
                            Color32::from_rgba_unmultiplied(t.color[0], t.color[1], t.color[2], t.color[3]));
                    }
                    // Selection handles are UI-only and NEVER exported into the image/video.
                    if self.selected_text == Some(index) && self.tool != Tool::Eraser
                        && self.inline_edit.is_none() {
                        let [_, _, width, height] = Self::text_bounds(t, [output_width, output_height]);
                        let bounds = Rect::from_min_size(pos,
                            Vec2::new(width * scale_x, height * scale_y)).expand(3.0);
                        ui.painter().rect_stroke(bounds, 2.0,
                            Stroke::new(1.5_f32, Color32::LIGHT_BLUE));
                        // One large bottom-right handle is the actual font-resize grip.
                        // The outline/handle stay out of saved screenshots/MP4s.
                        ui.painter().rect_filled(
                            Rect::from_center_size(bounds.right_bottom(), Vec2::splat(11.0)),
                            2.0, Color32::LIGHT_BLUE);
                        ui.painter().line_segment(
                            [bounds.right_bottom() - Vec2::splat(3.0),
                             bounds.right_bottom() + Vec2::splat(3.0)],
                            Stroke::new(1.0, Color32::BLACK));
                    }
                }
            }
        }
        if !self.active_points.is_empty() {
            let pts: Vec<Pos2> = self.active_points.iter().map(|p|
                Pos2::new(rect.min.x + p[0] * rect.width(), rect.min.y + p[1] * rect.height())).collect();
            let color = if self.tool == Tool::Highlighter { self.color.gamma_multiply(0.40) } else { self.color };
            let thickness = self.brush_size * if self.tool == Tool::Highlighter { 3.0 } else { 1.0 };
            if pts.len() == 1 { ui.painter().circle_filled(pts[0], (thickness * scale * 0.5).max(1.0), color); }
            else { ui.painter().add(egui::Shape::line(pts, Stroke::new((thickness * scale).max(1.0), color))); }
        }
    }

    /// Rasterize the edits onto a transparent overlay. This exact overlay can
    /// be composited onto a screenshot or burned into every frame of a video.
    pub fn render_overlay(&self, width: u32, height: u32) -> Result<RgbaImage, String> {
        let mut overlay = RgbaImage::new(width, height);
        let font = if self.marks.iter().any(|m| matches!(m, Mark::Text(_))) {
            Some(load_windows_font()?)
        } else { None };
        for mark in &self.marks {
            match mark {
                Mark::Brush(stroke) => paint_stroke(&mut overlay, stroke),
                Mark::Text(label) => {
                    if let Some(font) = &font { paint_text(&mut overlay, font, label); }
                }
            }
        }
        Ok(overlay)
    }
}

/// All erase hit-testing uses native output pixels, so it behaves the same at
/// any preview zoom and for both screenshot and recorded-video editors.
fn output_point(pos: [f32; 2], size: [u32; 2]) -> [f32; 2] {
    [pos[0] * size[0] as f32, pos[1] * size[1] as f32]
}

fn point_segment_distance_squared(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let length_squared = dx * dx + dy * dy;
    let t = (if length_squared > f32::EPSILON {
        ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / length_squared
    } else { 0.0 }).clamp(0.0, 1.0);
    let x = p[0] - (a[0] + t * dx);
    let y = p[1] - (a[1] + t * dy);
    x * x + y * y
}

fn segment_distance_squared(a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) -> f32 {
    // Proper crossing: neither segment's endpoints have to be near the other.
    let cross = |p: [f32; 2], q: [f32; 2], r: [f32; 2]| -> f32 {
        (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
    };
    let ab_c = cross(a, b, c);
    let ab_d = cross(a, b, d);
    let cd_a = cross(c, d, a);
    let cd_b = cross(c, d, b);
    if ((ab_c > 0.0 && ab_d < 0.0) || (ab_c < 0.0 && ab_d > 0.0))
        && ((cd_a > 0.0 && cd_b < 0.0) || (cd_a < 0.0 && cd_b > 0.0)) {
        return 0.0;
    }
    point_segment_distance_squared(a, c, d)
        .min(point_segment_distance_squared(b, c, d))
        .min(point_segment_distance_squared(c, a, b))
        .min(point_segment_distance_squared(d, a, b))
}

fn eraser_hits_mark(mark: &Mark, from: [f32; 2], to: [f32; 2], size: [u32; 2], radius: f32) -> bool {
    match mark {
        Mark::Brush(stroke) => {
            let hit_radius = radius + stroke.thickness * 0.5;
            let limit = hit_radius * hit_radius;
            if stroke.points.len() == 1 {
                return point_segment_distance_squared(output_point(stroke.points[0], size), from, to) <= limit;
            }
            stroke.points.windows(2).any(|pair| {
                segment_distance_squared(from, to,
                    output_point(pair[0], size), output_point(pair[1], size)) <= limit
            })
        }
        Mark::Text(label) => {
            let [left, top, width, height] = Editor::text_bounds(label, size);
            let right = left + width;
            let bottom = top + height;
            let inside = |p: [f32; 2]| {
                p[0] >= left && p[0] <= right && p[1] >= top && p[1] <= bottom
            };
            if inside(from) || inside(to) { return true; }
            let edges = [
                ([left, top], [right, top]),
                ([right, top], [right, bottom]),
                ([right, bottom], [left, bottom]),
                ([left, bottom], [left, top]),
            ];
            edges.iter().any(|&(start, end)| {
                segment_distance_squared(from, to, start, end) <= radius * radius
            })
        }
    }
}

fn load_windows_font() -> Result<fontdue::Font, String> {
    let windows = std::env::var_os("WINDIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts = windows.join("Fonts");
    for name in ["segoeui.ttf", "arial.ttf", "calibri.ttf", "tahoma.ttf"] {
        if let Ok(data) = fs::read(fonts.join(name)) {
            if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
                return Ok(font);
            }
        }
    }
    Err("Windows font not found. Expected Segoe UI, Arial, Calibri, or Tahoma in the Windows Fonts folder.".into())
}

fn paint_stroke(img: &mut RgbaImage, stroke: &BrushStroke) {
    // Track touched pixels so a highlighter does not become opaque simply
    // because successive brush disks overlap during a single stroke.
    let mut touched = std::collections::HashSet::new();
    let w = img.width() as f32;
    let h = img.height() as f32;
    let radius = (stroke.thickness / 2.0).max(0.5);
    for pair in stroke.points.windows(2) {
        let x0 = pair[0][0] * w;
        let y0 = pair[0][1] * h;
        let x1 = pair[1][0] * w;
        let y1 = pair[1][1] * h;
        let distance = (x1 - x0).hypot(y1 - y0);
        let steps = (distance / (radius * 0.65).max(1.0)).ceil() as u32;
        for step in 0..=steps.max(1) {
            let frac = step as f32 / steps.max(1) as f32;
            paint_disk(img, x0 + (x1 - x0) * frac, y0 + (y1 - y0) * frac, radius, stroke.color, &mut touched);
        }
    }
    if stroke.points.len() == 1 {
        let p = stroke.points[0];
        paint_disk(img, p[0] * w, p[1] * h, radius, stroke.color, &mut touched);
    }
}

fn paint_disk(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32, color: [u8; 4],
    touched: &mut std::collections::HashSet<(i32, i32)>) {
    let r = radius.ceil() as i32;
    for y in -r..=r {
        for x in -r..=r {
            let distance = (x as f32).hypot(y as f32);
            if distance <= radius {
                let px = cx.round() as i32 + x;
                let py = cy.round() as i32 + y;
                if touched.insert((px, py)) { blend(img, px, py, color, 1.0); }
            }
        }
    }
}

fn paint_text(img: &mut RgbaImage, font: &fontdue::Font, mark: &TextMark) {
    let size = mark.size;
    let start_x = mark.at[0] * img.width() as f32;
    let mut baseline = mark.at[1] * img.height() as f32 + size;
    let mut cursor_x = start_x;
    let mut last_char = None;
    for ch in mark.content.chars() {
        if ch == '\n' {
            cursor_x = start_x;
            baseline += size * 1.25;
            last_char = None;
            continue;
        }
        if let Some(prev) = last_char {
            cursor_x += font.horizontal_kern(prev, ch, size).unwrap_or(0.0);
        }
        let (metric, bitmap) = font.rasterize(ch, size);
        let left = cursor_x.round() as i32 + metric.xmin;
        let top = (baseline - metric.height as f32 - metric.ymin as f32).round() as i32;
        for row in 0..metric.height {
            for col in 0..metric.width {
                let coverage = bitmap[row * metric.width + col] as f32 / 255.0;
                if coverage > 0.0 {
                    blend(img, left + col as i32, top + row as i32, mark.color, coverage);
                }
            }
        }
        cursor_x += metric.advance_width;
        last_char = Some(ch);
    }
}

fn blend(img: &mut RgbaImage, x: i32, y: i32, color: [u8; 4], coverage: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 { return; }
    let dst: &mut Rgba<u8> = img.get_pixel_mut(x as u32, y as u32);
    let sa = color[3] as f32 / 255.0 * coverage;
    let da = dst[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 { return; }
    for i in 0..3 {
        let out = (color[i] as f32 * sa + dst[i] as f32 * da * (1.0 - sa)) / oa;
        dst[i] = out.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placed_text_can_be_moved_reworded_and_undone() {
        let size = [1920, 1080];
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Original".into();
        editor.click([0.10, 0.20], size).unwrap();
        assert!(editor.has_selected_text());

        editor.tool = Tool::Pen;
        editor.begin_drag([0.10, 0.20], size, 1.0);
        editor.continue_drag([0.30, 0.40], size);
        editor.end_drag();
        let Mark::Text(moved) = &editor.marks[0] else { panic!("expected text"); };
        assert!((moved.at[0] - 0.30).abs() < 0.001);
        assert!((moved.at[1] - 0.40).abs() < 0.001);

        editor.edit_text_input = "Changed wording".into();
        editor.edit_text_size = 70.0;
        editor.edit_text_color = Color32::RED;
        editor.apply_selected_text().unwrap();
        let Mark::Text(edited) = &editor.marks[0] else { panic!("expected text"); };
        assert_eq!(edited.content, "Changed wording");
        assert_eq!(edited.size, 70.0);
        assert_eq!(edited.color, Color32::RED.to_array());

        editor.undo(); // Undo the content/style edit, keep the new position.
        let Mark::Text(undone) = &editor.marks[0] else { panic!("expected text"); };
        assert_eq!(undone.content, "Original");
        assert!((undone.at[0] - 0.30).abs() < 0.001);
        editor.undo(); // Undo the movement.
        let Mark::Text(back_at_start) = &editor.marks[0] else { panic!("expected text"); };
        assert!((back_at_start.at[0] - 0.10).abs() < 0.001);
        assert!((back_at_start.at[1] - 0.20).abs() < 0.001);
    }

    #[test]
    fn selecting_text_does_not_add_another_label() {
        let size = [1280, 720];
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Hello".into();
        editor.click([0.20, 0.15], size).unwrap();
        editor.click([0.21, 0.16], size).unwrap();
        assert_eq!(editor.marks.len(), 1);
        editor.tool = Tool::Pen;
        editor.click([0.80, 0.80], size).unwrap();
        assert_eq!(editor.marks.len(), 1);
        assert!(!editor.has_selected_text());
    }

    #[test]
    fn pen_tool_can_select_move_and_resize_existing_text_without_drawing() {
        let image = [1000, 800];
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Resize me".into();
        editor.click([0.20, 0.20], image).unwrap();
        editor.tool = Tool::Pen;

        editor.begin_drag([0.205, 0.205], image, 1.0);
        editor.continue_drag([0.30, 0.30], image);
        editor.end_drag();
        let Mark::Text(moved) = &editor.marks[0] else { panic!("text was replaced"); };
        assert!(moved.at[0] > 0.28);
        assert_eq!(editor.marks.len(), 1, "moving text must not draw ink");

        let [left, top, width, height] = Editor::text_bounds(moved, image);
        let grip = [(left + width + 3.0) / image[0] as f32,
                    (top + height + 3.0) / image[1] as f32];
        assert!(editor.hovering_resize_handle(grip, image, 1.0));
        editor.begin_drag(grip, image, 1.0);
        editor.continue_drag([grip[0] + 0.09, grip[1] + 0.04], image);
        editor.end_drag();
        let Mark::Text(resized) = &editor.marks[0] else { panic!("expected text"); };
        assert!(resized.size > 48.0, "corner drag should increase font size");
        assert_eq!(editor.marks.len(), 1);
        editor.undo();
        let Mark::Text(unresized) = &editor.marks[0] else { panic!("expected text"); };
        assert!((unresized.size - 48.0).abs() < 0.001);
        editor.undo();
        let Mark::Text(at_start) = &editor.marks[0] else { panic!("expected text"); };
        assert!((at_start.at[0] - 0.20).abs() < 0.001);
    }

    #[test]
    fn inline_text_change_and_cancel_work_on_any_annotation_tool() {
        let image = [1000, 800];
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Original".into();
        editor.click([0.15, 0.15], image).unwrap();
        editor.tool = Tool::Highlighter;
        assert!(editor.start_inline_edit_at([0.17, 0.17], image));
        editor.inline_edit.as_mut().unwrap().draft = "Reworded".into();
        editor.finish_inline_edit(true).unwrap();
        let Mark::Text(mark) = &editor.marks[0] else { panic!("expected text"); };
        assert_eq!(mark.content, "Reworded");
        assert!(editor.start_inline_edit_at([0.17, 0.17], image));
        editor.inline_edit.as_mut().unwrap().draft = "Cancelled".into();
        editor.finish_inline_edit(false).unwrap();
        let Mark::Text(mark) = &editor.marks[0] else { panic!("expected text"); };
        assert_eq!(mark.content, "Reworded");
        editor.undo();
        let Mark::Text(mark) = &editor.marks[0] else { panic!("expected text"); };
        assert_eq!(mark.content, "Original");
    }

    #[test]
    fn eraser_click_removes_text_and_can_undo_and_redo() {
        let size = [1000, 700];
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Move me".into();
        editor.click([0.25, 0.30], size).unwrap();
        editor.tool = Tool::Eraser;
        editor.click([0.27, 0.32], size).unwrap();
        assert!(!editor.has_edits());
        assert!(!editor.has_selected_text());
        editor.undo();
        assert_eq!(editor.marks.len(), 1);
        editor.redo();
        assert!(editor.marks.is_empty());
    }

    #[test]
    fn eraser_hits_middle_of_sparse_stroke_not_just_saved_vertices() {
        let mut editor = Editor::default();
        editor.marks.push(Mark::Brush(BrushStroke {
            points: vec![[0.1, 0.25], [0.9, 0.25]],
            color: [255, 0, 0, 255], thickness: 4.0,
        }));
        editor.tool = Tool::Eraser;
        editor.eraser_size = 10.0;
        editor.click([0.5, 0.25], [1000, 1000]).unwrap();
        assert!(editor.marks.is_empty());
        editor.undo();
        assert_eq!(editor.marks.len(), 1);
    }

    #[test]
    fn eraser_drag_sweeps_over_highlighter_pen_and_text_in_one_undo_step() {
        let size = [1000, 1000];
        let mut editor = Editor::default();
        editor.marks.push(Mark::Brush(BrushStroke {
            points: vec![[0.20, 0.40], [0.30, 0.40]],
            color: [255, 0, 0, 255], thickness: 6.0,
        }));
        editor.marks.push(Mark::Text(TextMark {
            at: [0.45, 0.40], content: "Caption".into(),
            color: [255, 255, 255, 255], size: 45.0,
        }));
        editor.marks.push(Mark::Brush(BrushStroke {
            points: vec![[0.68, 0.40], [0.74, 0.40]],
            color: [0, 255, 0, 100], thickness: 20.0,
        }));
        editor.marks.push(Mark::Brush(BrushStroke {
            points: vec![[0.30, 0.80], [0.70, 0.80]],
            color: [0, 0, 255, 255], thickness: 5.0,
        }));
        editor.tool = Tool::Eraser;
        editor.eraser_size = 24.0;
        editor.begin_drag([0.10, 0.405], size, 1.0);
        editor.continue_drag([0.40, 0.405], size);
        editor.continue_drag([0.80, 0.405], size);
        editor.end_drag();
        assert_eq!(editor.marks.len(), 1, "only the distant ink should remain");
        assert_eq!(editor.undo_stack.len(), 1, "drag is one Undo action");
        editor.undo();
        assert_eq!(editor.marks.len(), 4);
        editor.redo();
        assert_eq!(editor.marks.len(), 1);
    }

    #[test]
    fn fast_eraser_drag_crosses_long_stroke_between_unsampled_points() {
        let size = [1000, 1000];
        let mut editor = Editor::default();
        editor.marks.push(Mark::Brush(BrushStroke {
            points: vec![[0.50, 0.20], [0.50, 0.80]],
            color: [20, 20, 20, 255], thickness: 2.0,
        }));
        editor.tool = Tool::Eraser;
        editor.eraser_size = 8.0;
        editor.begin_drag([0.10, 0.50], size, 1.0);
        editor.continue_drag([0.90, 0.50], size);
        editor.end_drag();
        assert!(editor.marks.is_empty(), "crossing segments must count as eraser hits");
    }

    #[test]
    fn eraser_does_not_remove_far_away_marks() {
        let size = [800, 600];
        let mut editor = Editor::default();
        editor.marks.push(Mark::Text(TextMark {
            at: [0.1, 0.1], content: "Safe".into(),
            color: [255, 255, 255, 255], size: 20.0,
        }));
        editor.tool = Tool::Eraser;
        editor.click([0.9, 0.9], size).unwrap();
        assert_eq!(editor.marks.len(), 1);
        assert!(!editor.can_undo(), "missing the target must not create undo history");
    }

    #[test]
    fn empty_text_edits_are_rejected_and_delete_is_undoable() {
        let mut editor = Editor::default();
        editor.tool = Tool::Text;
        editor.text_input = "Keep".into();
        editor.click([0.10, 0.10], [800, 600]).unwrap();
        editor.edit_text_input.clear();
        assert!(editor.apply_selected_text().is_err());
        editor.delete_selected_text();
        assert!(editor.marks.is_empty());
        editor.undo();
        assert_eq!(editor.marks.len(), 1);
    }
}
