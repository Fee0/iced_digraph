//! Interactive digraph heatmap (Iced + native file dialog).
//!
//! From this crate root (expects a sibling checkout of the `digraph` repo for the default sample path):
//!
//! ```text
//! cargo run
//! ```
//!
//! Controls: palette, overlapping vs non-overlapping pairs, normalization dropdown,
//! cell size slider, path, **Open…** (system file picker). The heatmap is
//! shown as a bitmap via [`Image`](iced::widget::Image) and [`Handle::from_rgba`](iced::widget::image::Handle::from_rgba).
//! File dialog and disk reads run on a worker thread via [`tokio::task::spawn_blocking`](tokio::task::spawn_blocking)
//! and return through [`Task::perform`](iced::Task::perform) so the UI thread stays responsive.
//! [`Scale::ClipPercentile`](digraph::normalize::Scale::ClipPercentile) is not exposed in this example yet.

use digraph::normalize::Scale;
use digraph::render::{render_rgba_pixels, RenderParams};
use digraph::{Digraph, HeatmapPalette, Mode};
use iced::widget::image::Handle;
use iced::widget::{button, column, pick_list, row, slider, text, text_input, Image};
use iced::{Element, Fill, Length, Task, Theme};
use std::path::PathBuf;

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
    Path(String),
    PickFile,
    FileLoaded(Result<(String, Vec<u8>), String>),
}

struct App {
    path: String,
    bytes: Vec<u8>,
    mode: Mode,
    palette: HeatmapPalette,
    scale: Scale,
    /// Slider value 1..=8 (cell pixels per digraph cell edge).
    cell_slider: f32,
    status: String,
    image: Handle,
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
        let bytes = std::fs::read(&path).unwrap_or_default();
        let mut s = Self {
            path,
            bytes,
            mode: Mode::NonOverlapping,
            palette: HeatmapPalette::default(),
            scale: Scale::CantorDust,
            cell_slider: 2.0,
            status: String::new(),
            image: Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        };
        s.recompute_image();
        if s.bytes.is_empty() {
            s.status = format!("No data (could not read {}).", s.path);
        } else {
            s.status = format!("Loaded {} bytes.", s.bytes.len());
        }
        (s, Task::none())
    }

    fn recompute_image(&mut self) {
        let d = Digraph::from_bytes_with_mode(&self.bytes, self.mode);
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
                        .await.unwrap_or_else(|e| Err(format!("task: {e}")))
                },
                Message::FileLoaded,
            ),
            Message::FileLoaded(res) => {
                match res {
                    Ok((path, bytes)) => {
                        self.path = path;
                        self.bytes = bytes;
                        self.status = format!("Loaded {} bytes.", self.bytes.len());
                        self.recompute_image();
                    }
                    Err(e) => self.status = e,
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let palette_pick = pick_list(PALETTES, Some(self.palette), Message::Palette);
        let mode_pick = pick_list(MODES, Some(self.mode), Message::Mode);
        let scale_pick = pick_list(SCALES, Some(self.scale), Message::Scale);
        let cell = self.cell_slider.round().clamp(1.0, 8.0) as u32;

        let controls = column![
            text("Palette").size(14),
            palette_pick,
            text("Pair mode").size(14),
            mode_pick,
            text("Normalization").size(14),
            scale_pick,
            text(format!("Cell size: {cell}px")).size(14),
            slider(1.0..=8.0, self.cell_slider, Message::Cell),
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

        row![
            controls,
            iced::widget::container(img)
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill),
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
