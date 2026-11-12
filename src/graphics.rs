use std::io::{self, Cursor, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};

const PAYLOAD_CHUNK_SIZE: usize = 4096;
// Kitty and Ghostty reject larger PNG dimensions, regardless of placement crop.
pub const MAX_IMAGE_DIMENSION: u32 = 10_000;
// The low bit selects one of two terminal image slots; the remaining bits
// identify the raster generation. Keep the old placement until its replacement
// has finished uploading, then release its pixels.
static NEXT_FRAME: AtomicU64 = AtomicU64::new(2);
static UPLOADED_FRAME: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub columns: u16,
    /// Total terminal rows, including the header and status rows.
    pub rows: u16,
    /// Rows reserved above the canvas (one only when the tab bar is visible).
    pub top: u16,
    /// Drawable pixel dimensions, excluding tab bar and status rows.
    pub width: u32,
    pub height: u32,
}

pub fn viewport(top: u16) -> Result<Viewport> {
    let (columns, rows, pixel_width, pixel_height) = match terminal::window_size() {
        Ok(size) => (size.columns, size.rows, size.width, size.height),
        Err(_) => {
            let (columns, rows) = terminal::size().context("reading terminal size")?;
            (columns, rows, 0, 0)
        }
    };
    if columns == 0 || rows <= top.saturating_add(1) {
        bail!("terminal is too small for the tab bar, document, and status row");
    }
    let cell_width = (u32::from(pixel_width) / u32::from(columns)).max(1);
    let cell_height = (u32::from(pixel_height) / u32::from(rows)).max(1);
    let cell_width = if pixel_width < columns { 8 } else { cell_width };
    let cell_height = if pixel_height < rows { 16 } else { cell_height };
    let height = u32::from(rows - top - 1) * cell_height;
    if height > MAX_IMAGE_DIMENSION {
        bail!("terminal drawable height exceeds the {MAX_IMAGE_DIMENSION}-pixel graphics limit");
    }
    Ok(Viewport {
        columns,
        rows,
        top,
        width: u32::from(columns) * cell_width,
        height,
    })
}

/// Restores the terminal on both errors and unwinding panics.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        if std::env::var_os("TMUX").is_some() {
            bail!("Kitty graphics inside tmux are not supported; run md-term outside tmux");
        }
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("interactive viewing requires a terminal; use --export for PNG output");
        }
        terminal::enable_raw_mode().context("enabling terminal raw mode")?;
        // Construct before any fallible output so partial initialization is cleaned up.
        let guard = Self;
        UPLOADED_FRAME.store(0, Ordering::Relaxed);
        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            DisableLineWrap,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )
        .context("initializing terminal")?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        UPLOADED_FRAME.store(0, Ordering::Relaxed);
        // Restore input first, even if writing to a disconnected terminal fails.
        let _ = terminal::disable_raw_mode();
        let mut out = io::stdout();
        for image_id in 1..=2 {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\");
        }
        let _ = execute!(
            out,
            DisableMouseCapture,
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        );
        let _ = out.flush();
    }
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    encoded_png: String,
    generation: u64,
}

impl Frame {
    /// Reads PNG dimensions without decoding pixels and caches its encoded payload.
    pub fn from_png(png: Vec<u8>) -> Result<Self> {
        let (width, height) =
            image::ImageReader::with_format(Cursor::new(&png), image::ImageFormat::Png)
                .into_dimensions()
                .context("reading viewport PNG dimensions")?;
        if width == 0 || height == 0 {
            bail!("viewport image has zero dimensions");
        }
        if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
            bail!("viewport image exceeds the {MAX_IMAGE_DIMENSION}-pixel graphics limit");
        }
        Ok(Self {
            width,
            height,
            encoded_png: STANDARD.encode(png),
            generation: NEXT_FRAME.fetch_add(2, Ordering::Relaxed),
        })
    }

    /// Draws a clamped native-pixel crop without touching header/status cells.
    /// Smaller images are centered on each axis inside the drawable area.
    pub fn draw(&self, out: &mut impl Write, view: Viewport, x: u32, y: u32) -> Result<()> {
        if view.columns == 0
            || view.rows <= view.top.saturating_add(1)
            || view.width == 0
            || view.height == 0
        {
            bail!("cannot draw into an empty viewport");
        }
        let crop = crop(self.width, self.height, view, x, y);
        let cell_width = (view.width / u32::from(view.columns)).max(1);
        let cell_height = (view.height / u32::from(view.rows - view.top - 1)).max(1);
        let previous = UPLOADED_FRAME.load(Ordering::Relaxed);
        let cached = previous & !1 == self.generation;
        let slot = if cached {
            previous & 1
        } else {
            (previous & 1) ^ 1
        };
        let image_id = slot + 1;
        execute!(
            out,
            MoveTo(
                (crop.left / cell_width) as u16,
                view.top + (crop.top / cell_height) as u16
            )
        )?;
        if cached {
            write!(
                out,
                "\x1b_Ga=p,i={image_id},p=1,x={},y={},w={},h={},X={},Y={},C=1,q=2\x1b\\",
                crop.x,
                crop.y,
                crop.width,
                crop.height,
                crop.left % cell_width,
                crop.top % cell_height,
            )?;
        } else {
            // A combined upload/placement is displayed only once all chunks
            // arrive. Never remove the visible raster before that point.
            UPLOADED_FRAME.store(0, Ordering::Relaxed);
            let chunks = self.encoded_png.as_bytes().chunks(PAYLOAD_CHUNK_SIZE);
            let count = chunks.len();
            for (index, chunk) in chunks.enumerate() {
                let more = u8::from(index + 1 < count);
                if index == 0 {
                    write!(
                        out,
                        "\x1b_Ga=T,t=d,f=100,i={image_id},p=1,x={},y={},w={},h={},X={},Y={},C=1,q=2,m={more};",
                        crop.x,
                        crop.y,
                        crop.width,
                        crop.height,
                        crop.left % cell_width,
                        crop.top % cell_height,
                    )?;
                } else {
                    write!(out, "\x1b_Gq=2,m={more};")?;
                }
                out.write_all(chunk)?;
                out.write_all(b"\x1b\\")?;
            }
            if previous != 0 {
                let previous_id = (previous & 1) + 1;
                write!(out, "\x1b_Ga=d,d=I,i={previous_id},q=2\x1b\\")?;
            }
        }
        out.flush()?;
        UPLOADED_FRAME.store(self.generation | slot, Ordering::Relaxed);
        Ok(())
    }
}

/// Removes placements only; cached image pixels remain available to Frame::draw.
pub fn clear(out: &mut impl Write) -> io::Result<()> {
    placements_clear(out)
}

pub fn placements_clear(out: &mut impl Write) -> io::Result<()> {
    let uploaded = UPLOADED_FRAME.load(Ordering::Relaxed);
    if uploaded != 0 {
        let image_id = (uploaded & 1) + 1;
        write!(out, "\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")?;
    }
    out.flush()
}

#[derive(Debug, Eq, PartialEq)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    left: u32,
    top: u32,
}

fn crop(width: u32, height: u32, view: Viewport, x: u32, y: u32) -> Crop {
    Crop {
        x: x.min(width.saturating_sub(view.width)),
        y: y.min(height.saturating_sub(view.height)),
        width: width.min(view.width),
        height: height.min(view.height),
        left: view.width.saturating_sub(width) / 2,
        top: view.height.saturating_sub(height) / 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Viewport {
        Viewport {
            columns: 100,
            rows: 32,
            top: 1,
            width: 800,
            height: 480,
        }
    }

    #[test]
    fn oversized_crop_clamps_at_both_edges() {
        assert_eq!(
            crop(1200, 1600, view(), u32::MAX, u32::MAX),
            Crop {
                x: 400,
                y: 1120,
                width: 800,
                height: 480,
                left: 0,
                top: 0,
            }
        );
    }

    #[test]
    fn small_image_ignores_scroll_and_centers_without_scaling() {
        assert_eq!(
            crop(301, 201, view(), 10, 20),
            Crop {
                x: 0,
                y: 0,
                width: 301,
                height: 201,
                left: 249,
                top: 139,
            }
        );
    }

    #[test]
    fn exact_fit_never_reaches_reserved_rows() {
        assert_eq!(
            crop(800, 480, view(), 1, 1),
            Crop {
                x: 0,
                y: 0,
                width: 800,
                height: 480,
                left: 0,
                top: 0,
            }
        );
    }

    #[derive(Default)]
    struct TerminalImages {
        images: std::collections::HashMap<u64, image::RgbaImage>,
        placements: std::collections::HashMap<u64, (u32, u32, u32, u32)>,
        pending: Option<(u64, (u32, u32, u32, u32), String)>,
        uploads: usize,
    }

    impl TerminalImages {
        fn apply(&mut self, output: &[u8], must_remain_visible: bool) {
            for command in output.split(|byte| *byte == 0x1b) {
                let Some(command) = command.strip_prefix(b"_G") else {
                    continue;
                };
                let command = std::str::from_utf8(command).unwrap();
                let (control, payload) = command.split_once(';').unwrap_or((command, ""));
                let fields: std::collections::HashMap<_, _> = control
                    .split(',')
                    .filter_map(|field| field.split_once('='))
                    .collect();
                let number = |key| fields.get(key).unwrap_or(&"0").parse::<u64>().unwrap();
                let id = number("i");
                let area = (
                    number("x") as u32,
                    number("y") as u32,
                    number("w") as u32,
                    number("h") as u32,
                );
                match fields.get("a").copied() {
                    Some("T") | Some("t") => {
                        self.images.remove(&id);
                        self.placements.remove(&id);
                        self.pending = Some((id, area, String::new()));
                        self.uploads += 1;
                    }
                    Some("p") => {
                        assert!(self.images.contains_key(&id), "placing missing pixels");
                        self.placements.insert(id, area);
                    }
                    Some("d") => {
                        assert!(self.pending.is_none(), "deletion aborts a chunked upload");
                        self.placements.remove(&id);
                        if fields.get("d") == Some(&"I") {
                            self.images.remove(&id);
                        }
                    }
                    _ => {}
                }
                if let Some((_, _, data)) = &mut self.pending {
                    data.push_str(payload);
                    if number("m") == 0 {
                        let (id, area, data) = self.pending.take().unwrap();
                        let png = STANDARD.decode(data).unwrap();
                        let pixels = image::load_from_memory(&png).unwrap().to_rgba8();
                        self.images.insert(id, pixels);
                        self.placements.insert(id, area);
                    }
                }
                if must_remain_visible {
                    assert!(!self.placements.is_empty(), "redraw exposed a blank canvas");
                }
                assert!(self.images.len() <= 2, "unbounded terminal raster storage");
            }
        }

        fn visible_pixel(&self) -> [u8; 4] {
            let (id, &(x, y, _, _)) = self.placements.iter().next().unwrap();
            self.images[id].get_pixel(x, y).0
        }
    }

    #[test]
    fn replacements_and_cached_crops_keep_pixels_visible_and_bounded() {
        let make_frame = |seed: u32| {
            let mut pixels = resvg::tiny_skia::Pixmap::new(64, 960).unwrap();
            let mut state = seed;
            for pixel in pixels.data_mut().chunks_exact_mut(4) {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                pixel.copy_from_slice(&[
                    (state >> 24) as u8,
                    (state >> 16) as u8,
                    (state >> 8) as u8,
                    255,
                ]);
            }
            let png = pixels.encode_png().unwrap();
            let first = image::load_from_memory(&png).unwrap().to_rgba8();
            let frame = Frame::from_png(png).unwrap();
            assert!(frame.encoded_png.len() > PAYLOAD_CHUNK_SIZE);
            (frame, first)
        };
        let (first, first_pixels) = make_frame(1);
        let (second, second_pixels) = make_frame(2);
        let mut terminal = TerminalImages::default();
        let mut output = Vec::new();
        UPLOADED_FRAME.store(0, Ordering::Relaxed);

        first.draw(&mut output, view(), 0, 0).unwrap();
        terminal.apply(&output, false);
        assert_eq!(terminal.visible_pixel(), first_pixels.get_pixel(0, 0).0);
        assert_eq!(terminal.uploads, 1);

        output.clear();
        first.draw(&mut output, view(), 0, 400).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.visible_pixel(), first_pixels.get_pixel(0, 400).0);
        assert_eq!(terminal.uploads, 1);

        output.clear();
        second.draw(&mut output, view(), 0, 480).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.visible_pixel(), second_pixels.get_pixel(0, 480).0);
        assert_eq!(terminal.images.len(), 1);

        output.clear();
        clear(&mut output).unwrap();
        terminal.apply(&output, false);
        assert!(terminal.placements.is_empty());
        assert_eq!(terminal.images.len(), 1);
        output.clear();
        second.draw(&mut output, view(), 0, 120).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.visible_pixel(), second_pixels.get_pixel(0, 120).0);
        assert_eq!(terminal.uploads, 2);

        output.clear();
        first.draw(&mut output, view(), 0, 240).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.visible_pixel(), first_pixels.get_pixel(0, 240).0);
        assert_eq!(terminal.images.len(), 1);
        assert_eq!(terminal.uploads, 3);
        UPLOADED_FRAME.store(0, Ordering::Relaxed);
    }
}
