//! Interactive digraph heatmap (Iced + native file dialog).
//!
//! From this crate root (expects a sibling checkout of the `digraph` repo for the default sample path):
//!
//! ```text
//! cargo run
//! ```
//!
//! Controls: palette, overlapping vs non-overlapping pairs, normalization dropdown,
//! **Open…** (system file picker), and a **byte rail** beside the heatmap
//! (scales vertically to the viewport; packs `bytes_per_row` per band as color columns; fixed rail width;
//! false color by value; draggable caps) to choose which byte range is fed into the digraph. The heatmap is shown as a bitmap via
//! [`Image`](iced::widget::Image) and [`Handle::from_rgba`](iced::widget::image::Handle::from_rgba).
//! The file dialog runs on a worker thread via [`tokio::task::spawn_blocking`](tokio::task::spawn_blocking).
//! Large files are read incrementally via a [`Subscription`](iced::Subscription) and [`iced::stream::channel`](iced::stream::channel).
//! [`Scale::ClipPercentile`](digraph::Scale::ClipPercentile) is not exposed in this example yet.

use digraph::{render_rgba_pixels, Digraph, HeatmapPalette, Mode, RenderParams, Scale};
use iced::futures::sink::SinkExt;
use iced::widget::image::Handle;
use iced::widget::{button, canvas, column, container, pick_list, progress_bar, row, text, Image};
use iced::futures::Stream;
use iced::{Element, Fill, Length, Size, Subscription, Task, Theme};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

mod minimap;

const TITLE: &str = "Digraph viewer";

/// Byte rail width in logical pixels (used by the minimap canvas).
const RAIL_ROW_WIDTH: f32 = 100.0;

/// Bytes read from disk per subscription chunk.
const READ_CHUNK: usize = 1024 * 1024;

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
    PickFile,
    /// Path from the native dialog only (file body loads via [`App::subscription`]).
    FilePickResult(Result<String, String>),
    FileLoadMeta {
        total_len: usize,
    },
    FileLoadChunk(Vec<u8>),
    FileLoadDone,
    FileLoadErr(String),
    RangeChanged(minimap::RangeChanged),
}

impl From<minimap::RangeChanged> for Message {
    fn from(r: minimap::RangeChanged) -> Self {
        Message::RangeChanged(r)
    }
}

/// Subscription identity: changing this cancels the previous file read stream.
#[derive(Clone)]
struct LoadStreamKey {
    gen: u64,
    path: PathBuf,
}

impl PartialEq for LoadStreamKey {
    fn eq(&self, other: &Self) -> bool {
        self.gen == other.gen && self.path == other.path
    }
}

impl Eq for LoadStreamKey {}

impl Hash for LoadStreamKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.gen.hash(state);
        self.path.hash(state);
    }
}

fn file_read_stream(key: &LoadStreamKey) -> impl Stream<Item = Message> + use<> {
    let path = key.path.clone();
    iced::stream::channel(64, async move |mut output| {
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                let _ = output
                    .send(Message::FileLoadErr(format!("metadata: {e}")))
                    .await;
                return;
            }
        };

        let total_len = match usize::try_from(meta.len()) {
            Ok(n) => n,
            Err(_) => {
                let _ = output
                    .send(Message::FileLoadErr(
                        "file is larger than this platform's address space".into(),
                    ))
                    .await;
                return;
            }
        };

        if output.send(Message::FileLoadMeta { total_len }).await.is_err() {
            return;
        }

        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = output.send(Message::FileLoadErr(e.to_string())).await;
                return;
            }
        };

        let mut scratch = vec![0u8; READ_CHUNK];
        loop {
            let n = match tokio::io::AsyncReadExt::read(&mut file, &mut scratch).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = output.send(Message::FileLoadErr(e.to_string())).await;
                    return;
                }
            };
            if n == 0 {
                break;
            }
            let chunk = scratch[..n].to_vec();
            if output.send(Message::FileLoadChunk(chunk)).await.is_err() {
                return;
            }
        }

        let _ = output.send(Message::FileLoadDone).await;
    })
}

struct App {
    path: String,
    bytes: Arc<RwLock<Vec<u8>>>,
    /// Declared file size from metadata (selection + rail span). Equals `bytes.len()` when load is complete.
    file_total_len: usize,
    /// When `Some`, [`file_read_stream`] is active for this generation and path.
    active_load: Option<LoadStreamKey>,
    /// Increments for each new file read stream (subscription identity).
    load_gen: u64,
    mode: Mode,
    palette: HeatmapPalette,
    scale: Scale,
    /// Slider value 1..=8 (cell pixels per digraph cell edge).
    cell_slider: f32,
    /// File bytes represented by one horizontal strip row (reduces vertical scroll).
    rail_bytes_per_row: u32,
    /// Bumps when bytes or rail layout change; invalidates canvas strip cache.
    rail_strip_generation: u64,
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
        let raw = std::fs::read(&path).unwrap_or_default();
        let file_total_len = raw.len();
        let bytes = Arc::new(RwLock::new(raw));
        let mut s = Self {
            path,
            bytes,
            file_total_len,
            active_load: None,
            load_gen: 0,
            mode: Mode::NonOverlapping,
            palette: HeatmapPalette::default(),
            scale: Scale::CantorDust,
            cell_slider: 2.0,
            rail_bytes_per_row: 64,
            rail_strip_generation: 1,
            image: Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
            range_start_norm: 0.0,
            range_end_norm: 1.0,
        };
        s.recompute_image();
        (s, Task::none())
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.active_load {
            Some(key) => Subscription::run_with(key.clone(), file_read_stream),
            None => Subscription::none(),
        }
    }

    fn loaded_len(&self) -> usize {
        self.bytes.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Progress 0..=100 while a chunked file load is active (`active_load`).
    fn load_percent(&self) -> f32 {
        if self.file_total_len == 0 {
            return 0.0;
        }
        let loaded = self.loaded_len() as f32;
        let total = self.file_total_len as f32;
        (100.0 * loaded / total.max(1.0)).clamp(0.0, 100.0)
    }

    /// Byte span for selection handles, in logical file coordinates.
    fn selection_span_bytes(&self) -> usize {
        if self.file_total_len > 0 {
            self.file_total_len
        } else if self.active_load.is_some() {
            0
        } else {
            self.loaded_len().max(1)
        }
    }

    fn selected_byte_range(&self) -> (usize, usize) {
        let n = self.selection_span_bytes();
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
        if self.active_load.is_some() {
            return;
        }
        let (lo, hi) = self.selected_byte_range();
        let guard = match self.bytes.read() {
            Ok(g) => g,
            Err(_) => {
                self.image = Handle::from_rgba(1, 1, vec![0, 0, 0, 255]);
                return;
            }
        };
        let loaded = guard.len();
        let hi = hi.min(loaded);
        let lo = lo.min(hi);
        let slice = if hi > lo { &guard[lo..hi] } else { &[][..] };
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
            Message::PickFile => Task::perform(
                async {
                    tokio::task::spawn_blocking(|| {
                        let p = rfd::FileDialog::new()
                            .set_title("Open binary")
                            .pick_file()
                            .ok_or_else(|| "canceled".to_string())?;
                        Ok::<_, String>(p.to_string_lossy().to_string())
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("task: {e}")))
                },
                Message::FilePickResult,
            ),
            Message::FilePickResult(res) => {
                match res {
                    Ok(path_str) => {
                        let path = PathBuf::from(&path_str);
                        self.path = path_str;
                        self.range_start_norm = 0.0;
                        self.range_end_norm = 1.0;
                        self.file_total_len = 0;
                        if let Ok(mut g) = self.bytes.write() {
                            g.clear();
                        }
                        self.load_gen = self.load_gen.wrapping_add(1);
                        self.active_load = Some(LoadStreamKey {
                            gen: self.load_gen,
                            path,
                        });
                        self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                    }
                    Err(_) => {}
                }
                Task::none()
            }
            Message::FileLoadMeta { total_len } => {
                self.file_total_len = total_len;
                let reserve_ok = match self.bytes.write() {
                    Ok(mut g) => {
                        g.clear();
                        g.try_reserve_exact(total_len).is_ok()
                    }
                    Err(_) => false,
                };
                if !reserve_ok {
                    self.active_load = None;
                    self.file_total_len = 0;
                    self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                    self.recompute_image();
                    return Task::none();
                }
                self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                Task::none()
            }
            Message::FileLoadChunk(chunk) => {
                if let Ok(mut g) = self.bytes.write() {
                    let room = self.file_total_len.saturating_sub(g.len());
                    let take = chunk.len().min(room);
                    if take > 0 {
                        g.extend_from_slice(&chunk[..take]);
                    }
                }
                Task::none()
            }
            Message::FileLoadDone => {
                self.active_load = None;
                self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                self.recompute_image();
                Task::none()
            }
            Message::FileLoadErr(e) => {
                let _ = e;
                self.active_load = None;
                if let Ok(mut g) = self.bytes.write() {
                    g.clear();
                }
                self.file_total_len = 0;
                self.rail_strip_generation = self.rail_strip_generation.wrapping_add(1);
                self.recompute_image();
                Task::none()
            }
            Message::RangeChanged(r) => {
                self.range_start_norm = r.start.clamp(0.0, 1.0);
                self.range_end_norm = r.end.clamp(0.0, 1.0);
                self.recompute_image();
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let palette_pick = pick_list(PALETTES, Some(self.palette), Message::Palette);
        let mode_pick = pick_list(MODES, Some(self.mode), Message::Mode);
        let scale_pick = pick_list(SCALES, Some(self.scale), Message::Scale);
        let row_w = RAIL_ROW_WIDTH.clamp(2.0, 256.0);
        let bpr = self.rail_bytes_per_row.max(1);

        let mut controls = column![
            text("Palette").size(14),
            palette_pick,
            text("Pair mode").size(14),
            mode_pick,
            text("Normalization").size(14),
            scale_pick,
            button("Open…").on_press(Message::PickFile),
        ]
        .spacing(8)
        .width(Length::Fixed(300.0));

        if self.active_load.is_some() {
            if self.file_total_len == 0 {
                controls = controls.push(text("Reading file size…").size(12));
            }
            controls = controls.push(
                progress_bar(0.0..=100.0, self.load_percent())
                    .length(Fill)
                    .girth(Length::Fixed(8.0)),
            );
        }

        let img = Image::new(self.image.clone())
            .width(Fill)
            .height(Fill)
            .content_fit(iced::ContentFit::Contain);

        let rail_logical_len = if self.file_total_len > 0 {
            self.file_total_len
        } else if self.active_load.is_some() {
            1
        } else {
            self.loaded_len()
        };

        let rail_canvas = canvas(minimap::ByteRangeRail {
            range_start_norm: self.range_start_norm,
            range_end_norm: self.range_end_norm,
            bytes: self.bytes.clone(),
            total_len: rail_logical_len,
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
        .subscription(App::subscription)
        .theme(App::theme)
        .title(TITLE)
        .window_size(Size::new(1420.0, 1000.0))
        .run()
}
