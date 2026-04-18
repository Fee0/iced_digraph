//! Vertical byte-range rail: strip bands scale to the canvas viewport height (no internal scroll).
//! Each band packs `bytes_per_row` consecutive bytes as horizontal color segments; width is
//! configurable; banded false color; cached strip geometry; dual-handle selection.
//! Large files auto-raise effective bytes-per-row and merge horizontal samples so solid-fill
//! tessellation stays under typical GPU buffer limits (~256 MiB).

use std::cell::Cell;
use std::sync::{Arc, RwLock};

use iced::mouse;
use iced::widget::canvas::{Action, Cache, Event, Frame, Geometry, Path, Program, Stroke};
use iced::widget::canvas::{LineCap, LineJoin};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

/// Number of horizontal strip bands for a file length and `bytes_per_row` packing.
#[inline]
pub fn strip_row_count(byte_len: usize, bytes_per_row: u32) -> usize {
    let b = bytes_per_row.max(1) as usize;
    if byte_len == 0 {
        1
    } else {
        byte_len.div_ceil(b)
    }
}

/// Max vertical bands drawn per frame (avoids huge iced/wgpu solid triangle vertex buffers).
const MAX_DRAW_STRIP_ROWS: usize = 2048;
/// Max horizontal rectangles per band (extra bytes merged via average color).
const MAX_SEGMENTS_PER_ROW: usize = 128;

#[inline]
fn effective_bytes_per_row(byte_len: usize, bpr: usize) -> usize {
    let bpr = bpr.max(1);
    if byte_len == 0 {
        return 1;
    }
    if strip_row_count(byte_len, bpr.min(u32::MAX as usize) as u32) <= MAX_DRAW_STRIP_ROWS {
        bpr
    } else {
        bpr.max(byte_len.div_ceil(MAX_DRAW_STRIP_ROWS))
    }
}

#[inline]
fn average_byte(chunk: &[u8]) -> u8 {
    if chunk.is_empty() {
        return 0;
    }
    (chunk.iter().map(|&x| x as u32).sum::<u32>() / chunk.len() as u32) as u8
}

/// Fixed hit area half-thickness (px) for each horizontal handle.
const HANDLE_HALF: f32 = 6.0;

/// Map a byte to RGB in four broad bands across 0..=255.
pub fn byte_value_color(b: u8) -> Color {
    let t = b as f32 / 255.0;
    let (a, b_, u) = if t < 0.25 {
        let u = t * 4.0;
        (
            Color::from_rgb8(0x1a, 0x2e, 0x6e),
            Color::from_rgb8(0x22, 0x7a, 0xa8),
            u,
        )
    } else if t < 0.5 {
        let u = (t - 0.25) * 4.0;
        (
            Color::from_rgb8(0x22, 0x7a, 0xa8),
            Color::from_rgb8(0x2e, 0x9d, 0x4a),
            u,
        )
    } else if t < 0.75 {
        let u = (t - 0.5) * 4.0;
        (
            Color::from_rgb8(0x2e, 0x9d, 0x4a),
            Color::from_rgb8(0xd4, 0x8a, 0x2b),
            u,
        )
    } else {
        let u = (t - 0.75) * 4.0;
        (
            Color::from_rgb8(0xd4, 0x8a, 0x2b),
            Color::from_rgb8(0x8e, 0x2f, 0x7b),
            u,
        )
    };
    Color {
        r: a.r + (b_.r - a.r) * u,
        g: a.g + (b_.g - a.g) * u,
        b: a.b + (b_.b - a.b) * u,
        a: 1.0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragKind {
    Start,
    End,
    Range,
}

pub struct RailState {
    drag: Option<DragKind>,
    anchor_start: f32,
    anchor_end: f32,
    anchor_cursor_y: f32,
    strip_cache: Cache<Renderer>,
    /// Generation last baked into `strip_cache` (invalidation via [`Cell`] — `draw` is `&State`).
    strip_gen_baked: Cell<u64>,
}

impl Default for RailState {
    fn default() -> Self {
        Self {
            drag: None,
            anchor_start: 0.0,
            anchor_end: 0.0,
            anchor_cursor_y: 0.0,
            strip_cache: Cache::new(),
            strip_gen_baked: Cell::new(0),
        }
    }
}

/// Canvas: each y-row covers `bytes_per_row` file bytes as equal-width color columns.
#[derive(Clone)]
pub struct ByteRangeRail {
    pub range_start_norm: f32,
    pub range_end_norm: f32,
    pub bytes: Arc<RwLock<Vec<u8>>>,
    /// Logical file length (metadata). While `bytes.read().len() < total_len`, the strip is a placeholder.
    pub total_len: usize,
    pub row_width: f32,
    /// Consecutive file bytes represented by one horizontal strip row (>= 1).
    pub bytes_per_row: u32,
    /// Increment when strip parameters change; invalidates `strip_cache`.
    pub strip_generation: u64,
}

impl<M: From<RangeChanged>> Program<M> for ByteRangeRail {
    type State = RailState;

    fn update(
        &self,
        state: &mut RailState,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<M>> {
        let h = bounds.height.max(1.0);
        let n = self.total_len;
        let gap = min_gap(n);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let y = p.y;
                let y_start = self.range_start_norm * h;
                let y_end = self.range_end_norm * h;
                let (lo, hi) = if y_start <= y_end {
                    (y_start, y_end)
                } else {
                    (y_end, y_start)
                };

                let hit_start = (y - y_start).abs() <= HANDLE_HALF;
                let hit_end = (y - y_end).abs() <= HANDLE_HALF;

                if hit_start && !hit_end {
                    state.drag = Some(DragKind::Start);
                    return Some(Action::capture());
                }
                if hit_end && !hit_start {
                    state.drag = Some(DragKind::End);
                    return Some(Action::capture());
                }
                if hit_start && hit_end {
                    if (y - y_start).abs() <= (y - y_end).abs() {
                        state.drag = Some(DragKind::Start);
                    } else {
                        state.drag = Some(DragKind::End);
                    }
                    return Some(Action::capture());
                }
                if y >= lo && y <= hi {
                    state.drag = Some(DragKind::Range);
                    state.anchor_start = self.range_start_norm;
                    state.anchor_end = self.range_end_norm;
                    state.anchor_cursor_y = p.y;
                    return Some(Action::capture());
                }
                None
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let kind = state.drag?;
                let y_abs = cursor.position()?.y;
                let y = (y_abs - bounds.y).clamp(0.0, h);
                let norm = (y / h).clamp(0.0, 1.0);
                Some(match kind {
                    DragKind::Start => {
                        let (_, e0) = ordered(self.range_start_norm, self.range_end_norm);
                        let s = norm;
                        let e = if s >= e0 {
                            (s + gap).min(1.0)
                        } else {
                            e0
                        };
                        let (s, e) = ordered(s, e);
                        Action::publish(M::from(RangeChanged { start: s, end: e })).and_capture()
                    }
                    DragKind::End => {
                        let (s0, _) = ordered(self.range_start_norm, self.range_end_norm);
                        let e = norm;
                        let s = if e <= s0 {
                            (e - gap).max(0.0)
                        } else {
                            s0
                        };
                        let (s, e) = ordered(s, e);
                        Action::publish(M::from(RangeChanged { start: s, end: e })).and_capture()
                    }
                    DragKind::Range => {
                        let dy = (y - state.anchor_cursor_y) / h;
                        let w = state.anchor_end - state.anchor_start;
                        let mut s = state.anchor_start + dy;
                        let mut e = state.anchor_end + dy;
                        if s < 0.0 {
                            s = 0.0;
                            e = w.min(1.0);
                        }
                        if e > 1.0 {
                            e = 1.0;
                            s = (1.0 - w).max(0.0);
                        }
                        Action::publish(M::from(RangeChanged { start: s, end: e })).and_capture()
                    }
                })
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some() {
                    return Some(Action::capture());
                }
                None
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &RailState,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let w = bounds.width.max(1.0);
        let h = bounds.height.max(1.0);
        let n = self.total_len;
        let row_w = self.row_width.max(1.0);

        if state.strip_gen_baked.get() != self.strip_generation {
            state.strip_cache.clear();
            state.strip_gen_baked.set(self.strip_generation);
        }

        let palette = theme.palette();
        let bpr = self.bytes_per_row.max(1) as usize;
        let eff_bpr = effective_bytes_per_row(n, bpr);

        let strip_geo = state.strip_cache.draw(renderer, Size::new(w, h), |frame| {
            if n == 0 {
                frame.fill(
                    &Path::rectangle(Point::ORIGIN, Size::new(w, h)),
                    palette.background.scale_alpha(0.9),
                );
                return;
            }

            let guard = match self.bytes.read() {
                Ok(g) => g,
                Err(_) => {
                    frame.fill(
                        &Path::rectangle(Point::ORIGIN, Size::new(w, h)),
                        palette.background.scale_alpha(0.9),
                    );
                    return;
                }
            };
            if guard.len() < n {
                frame.fill(
                    &Path::rectangle(Point::ORIGIN, Size::new(w, h)),
                    palette.background.scale_alpha(0.75),
                );
                return;
            }

            let rows = strip_row_count(n, eff_bpr.min(u32::MAX as usize) as u32);
            let row_h = h / rows as f32;
            for r in 0..rows {
                let y0 = r as f32 * row_h;
                if y0 >= h {
                    break;
                }
                let lo = r * eff_bpr;
                if lo >= n {
                    break;
                }
                let hi = (lo + eff_bpr).min(n);
                let chunk = &guard[lo..hi];
                let len = chunk.len().max(1);
                let seg_count = len.clamp(1, MAX_SEGMENTS_PER_ROW);
                let seg_w = row_w / seg_count as f32;
                for s in 0..seg_count {
                    let t0 = s * len / seg_count;
                    let t1 = ((s + 1) * len / seg_count).min(len);
                    if t0 >= t1 {
                        continue;
                    }
                    let sub = &chunk[t0..t1];
                    let x = s as f32 * seg_w;
                    frame.fill_rectangle(
                        Point::new(x, y0),
                        Size::new(seg_w, row_h),
                        byte_value_color(average_byte(sub)),
                    );
                }
            }
            if row_w < w {
                frame.fill_rectangle(
                    Point::new(row_w, 0.0),
                    Size::new((w - row_w).max(0.0), h),
                    palette.background.scale_alpha(0.55),
                );
            }
        });

        let mut overlay = Frame::new(renderer, Size::new(w, h));
        let (s, e) = ordered(self.range_start_norm, self.range_end_norm);
        let y0 = s * h;
        let y1 = e * h;
        let band_top = y0.min(y1);
        let band_h = (y1 - y0).abs().max(1.0);
        overlay.fill_rectangle(
            Point::new(0.0, band_top),
            Size::new(w, band_h),
            palette.primary.scale_alpha(0.2),
        );

        let handle_fill = palette.text.scale_alpha(0.92);
        let handle_stroke = Stroke::default()
            .with_width(2.0)
            .with_color(palette.background.scale_alpha(0.95))
            .with_line_cap(LineCap::Round)
            .with_line_join(LineJoin::Round);
        for yn in [self.range_start_norm * h, self.range_end_norm * h] {
            let y = yn - HANDLE_HALF;
            let rect = Path::rectangle(Point::new(0.0, y), Size::new(w, HANDLE_HALF * 2.0));
            overlay.fill(&rect, handle_fill);
            overlay.stroke(&rect, handle_stroke);
        }

        vec![strip_geo, overlay.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &RailState,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.drag.is_some() {
            mouse::Interaction::Grabbing
        } else {
            mouse::Interaction::Crosshair
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RangeChanged {
    pub start: f32,
    pub end: f32,
}

fn ordered(a: f32, b: f32) -> (f32, f32) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn min_gap(byte_len: usize) -> f32 {
    (1.0 / byte_len.max(1) as f32).min(1.0 / 4096.0)
}
