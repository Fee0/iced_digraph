//! Interactive digraph heatmap (Iced + native file dialog).
//!
//! From this crate root (expects a sibling checkout of the `digraph` repo for the default sample path):
//!
//! ```text
//! cargo run
//! ```
//!
//! Controls: palette, overlapping vs non-overlapping pairs, normalization dropdown,
//! cell size slider, path, **Open…** (system file picker), and a **byte rail** beside the heatmap
//! (scales vertically to the viewport; packs `bytes_per_row` per band as color columns; configurable width;
//! false color by value; draggable caps) to choose which byte range is fed into the digraph. The heatmap is shown as a bitmap via
//! [`Image`](iced::widget::Image) and [`Handle::from_rgba`](iced::widget::image::Handle::from_rgba).
//! File dialog and disk reads run on a worker thread via [`tokio::task::spawn_blocking`](tokio::task::spawn_blocking)
//! and return through [`Task::perform`](iced::Task::perform) so the UI thread stays responsive.
//! [`Scale::ClipPercentile`](digraph::normalize::Scale::ClipPercentile) is not exposed in this example yet.

use digraph::normalize::Scale;
use digraph::render::{render_rgba_pixels, RenderParams};
use digraph::{Digraph, HeatmapPalette, Mode};
use iced::widget::image::Handle;
use iced::widget::{button, canvas, column, container, pick_list, row, slider, text, text_input, Image};
use iced::{Element, Fill, Length, Task, Theme};
use std::path::PathBuf;
use std::sync::Arc;

mod minimap;

const TITLE: &str = "Digraph viewer";

const PALETTES: &[HeatmapPalette] = &[
    HeatmapPalette::Magma,
    HeatmapPalette::Viridis,
    HeatmapPalette::Gray,
    HeatmapPalette::Matrix,
];

const MODES: &[Mode] = &[Mode::Overlapping, Mode::NonOverlapping];
const SCALES: &[Scale] = &[Scale::Log1p, Scale::CantorDust];

#[derive(Debug, Clone)]
enum Message {
    Palette(HeatmapPalette),
    Mode(Mode),
    Scale(Scale),
    Cell(f32),
    RailWidth(f32),
    RailBytesPerRow(f32),
    Path(String),
    PickFile,
    FileLoaded(Result<(String, Vec<u8>), String>),
    RangeChanged(minimap::RangeChanged),
}

impl From<minimap::RangeChanged> for Message {
    fn from(r: minimap::RangeChanged) -> Self {
        Message::RangeChanged(r)
    }
}

struct App {
    path: String,
    bytes: Arc<Vec<u8>>,
    mode: Mode,
    palette: HeatmapPalette,
    scale: Scale,
    /// Slider value 1..=8 (cell pixels per digraph cell edge).
    cell_slider: f32,
    /// Byte rail width in logical px (2..=256).
    rail_row_width: f32,
    /// File bytes represented by one horizontal strip row (reduces vertical scroll).
    rail_bytes_per_row: u32,
    /// Bumps when bytes or rail layout change; invalidates canvas strip cache.
    rail_strip_generation: u64,
    status: String,
    image: Handle,
    /// Selection along the full file, 0..1 (top = start of file).
    range_start_norm: f32,
    range_end_norm: f32,
}

impl App {
    /// Default sample: sibling repo `../digraph/tests/data/sample.bin`.
    fn default_path() -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../digraph/tests/data/sample.bin");
        p.to_string_lossy().to_string()
    }

    fn init() -> (Self, Task<Message>) {
        let path = Self::default_path();
        let bytes = Arc::new(std::fs::read(&path).unwrap_or_default());
        let mut s = Self {
            path,
            bytes,
            mode: Mode::NonOverlapping,
            palette: HeatmapPalette::default(),
            scale: Scale::CantorDust,
            cell_slider: 2.0,
            rail_row_width: 48.0,
            rail_bytes_per_row: 64,
            rail_strip_generation: 1,
            status: String::new(),
            image: Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
            range_start_norm: 0.0,
            range_end_norm: 1.0,
        };
        s.recompute_image();
        s.refresh_status();
        (s, Task::none())
    }

    fn refresh_status(&mut self) {
        if self.bytes.is_empty() {
            self.status = format!("No data (could not read {}).", self.path);
            return;
        }
        let n = self.bytes.len();
        let (lo, hi) = self.selected_byte_range();
        let mut msg = format!("Loaded {n} bytes. Selection [{lo}, {hi}).");
        let rows = minimap::strip_row_count(n, self.rail_bytes_per_row);
        if rows >= minimap::STRIP_ROW_WARN {
            msg.push_str(&format!(
                " Rail: {rows} strip rows (first paint may hitch; threshold {}).",
                minimap::STRIP_ROW_WARN
            ));
        }
        self.status = msg;
    }

    fn selected_byte_range(&self) -> (usize, usize) {
        let n = self.bytes.len();
        if n == 0 {
            return (0, 0);
        }
        let mut lo = (self.range_start_norm * n as f32).floor() as i64;
        let mut hi = (self.range_end_norm * n as f32).ceil() as i64;
        lo = lo.clamp(0, n as i64 - 1);
        hi = hi.clamp(1, n as i64);
        let lo = lo as usize;
        let mut hi = hi as usize;
        if hi <= lo {
            hi = (lo + 1).min(n);
        }
        (lo, hi)
    }

    fn recompute_image(&mut self) {
        let (lo, hi) = self.selected_byte_range();
        let slice = if hi > lo {
            &self.bytes[lo..hi]
        } else {
            &[][..]
        };
        let d = Digraph::from_bytes_with_mode(slice, self.mode);
        let params = RenderParams {
            cell_pixels: self.cell_slider.round().clamp(1.0, 8.0) as u32,
            scale: self.scale,
            palette: self.palette,
        };
        let pm = render_rgba_pixels(&d, params);
        self.image = Handle::from_rgba(pm.width, pm.height, pm.rgba);
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Palette(p) => {
                self.palette = p;
                self.recompute_image();
                Task::none()
            }
            Message::Mode(m) => {
                self.mode = m;
                self.recompute_image();
                Task::none()
            }
            Message::Scale(scale) => {
                self.scale = scale;
                self.recompute_image();
                Task::none()
            }
            Message::Cell(v) => {
                self.cell_slider = v;
                self.recompute_image();
                Task::none()
            }
            Message::RailWidth(v) => {
                self.rail_row_width = v.round().clamp(2.0, 256.0);
                self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                Task::none()
            }
            Message::RailBytesPerRow(v) => {
                self.rail_bytes_per_row = v.round().clamp(1.0, 65_536.0) as u32;
                self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                Task::none()
            }
            Message::Path(p) => {
                self.path = p;
                Task::none()
            }
            Message::PickFile => Task::perform(
                async {
                    tokio::task::spawn_blocking(|| {
                        let p = rfd::FileDialog::new()
                            .set_title("Open binary")
                            .pick_file()
                            .ok_or_else(|| "canceled".to_string())?;
                        let path = p.to_string_lossy().to_string();
                        let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
                        Ok::<_, String>((path, bytes))
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("task: {e}")))
                },
                Message::FileLoaded,
            ),
            Message::FileLoaded(res) => {
                match res {
                    Ok((path, bytes)) => {
                        self.path = path;
                        self.bytes = Arc::new(bytes);
                        self.range_start_norm = 0.0;
                        self.range_end_norm = 1.0;
                        self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                        self.recompute_image();
                        self.refresh_status();
                    }
                    Err(e) => self.status = e,
                }
                Task::none()
            }
            Message::RangeChanged(r) => {
                self.range_start_norm = r.start.clamp(0.0, 1.0);
                self.range_end_norm = r.end.clamp(0.0, 1.0);
                self.recompute_image();
                self.refresh_status();
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let palette_pick = pick_list(PALETTES, Some(self.palette), Message::Palette);
        let mode_pick = pick_list(MODES, Some(self.mode), Message::Mode);
        let scale_pick = pick_list(SCALES, Some(self.scale), Message::Scale);
        let cell = self.cell_slider.round().clamp(1.0, 8.0) as u32;
        let row_w = self.rail_row_width.round().clamp(2.0, 256.0);
        let rail_px = row_w as u32;
        let bpr = self.rail_bytes_per_row.max(1);

        let controls = column![
            text("Palette").size(14),
            palette_pick,
            text("Pair mode").size(14),
            mode_pick,
            text("Normalization").size(14),
            scale_pick,
            text(format!("Cell size: {cell}px")).size(14),
            slider(1.0..=8.0, self.cell_slider, Message::Cell),
            text(format!("Rail width: {rail_px}px")).size(14),
            slider(2.0..=256.0, self.rail_row_width, Message::RailWidth),
            text(format!("Bytes / strip row: {bpr}")).size(14),
            slider(1.0..=4096.0, self.rail_bytes_per_row as f32, Message::RailBytesPerRow),
            text("Path").size(14),
            text_input("Path", &self.path).on_input(Message::Path),
            button("Open…").on_press(Message::PickFile),
            text(&self.status).size(12),
        ]
        .spacing(8)
        .width(Length::Fixed(300.0));

        let img = Image::new(self.image.clone())
            .width(Fill)
            .height(Fill)
            .content_fit(iced::ContentFit::Contain);

        let rail_canvas = canvas(minimap::ByteRangeRail {
            range_start_norm: self.range_start_norm,
            range_end_norm: self.range_end_norm,
            bytes: self.bytes.clone(),
            row_width: row_w,
            bytes_per_row: bpr,
            strip_generation: self.rail_strip_generation,
        })
        .width(Length::Fixed(row_w))
        .height(Fill);

        let rail: Element<'_, Message> = container(rail_canvas)
            .width(Length::Fixed(row_w))
            .height(Fill)
            .into();

        let viewer = row![
            rail,
            iced::widget::container(img)
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill),
        ]
        .spacing(8);

        row![
            controls,
            viewer.width(Fill).height(Fill),
        ]
        .spacing(12)
        .padding(12)
        .into()
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn main() -> iced::Result {
    iced::application(App::init, App::update, App::view)
        .theme(App::theme)
        .title(TITLE)
        .run()
}
