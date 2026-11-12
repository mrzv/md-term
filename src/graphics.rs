use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
use image::ImageEncoder;
use resvg::tiny_skia;

const PAYLOAD_CHUNK_SIZE: usize = 4096;
// Kitty and Ghostty reject larger PNG dimensions, regardless of placement crop.
pub const MAX_IMAGE_DIMENSION: u32 = 10_000;
// Herdr forwards decoded RGBA assets, not compressed PNGs. Stay within its
// 16 MiB native-file budget and below its 32 MiB inline frame limit.
pub const MAX_IMAGE_PIXELS: u32 = 4 * 1024 * 1024;
const MAX_TILES: u32 = MAX_IMAGE_DIMENSION.div_ceil(MAX_IMAGE_PIXELS / MAX_IMAGE_DIMENSION);
// The low bit selects one of two terminal image slots; the remaining bits
// identify the raster generation. Keep the old placement until its replacement
// has finished uploading, then release its pixels.
static NEXT_FRAME: AtomicU64 = AtomicU64::new(2);
static UPLOADED_FRAME: AtomicU64 = AtomicU64::new(0);
static UPLOADED_TILES: AtomicU32 = AtomicU32::new(0);

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
        for image_id in 1..=2 * MAX_TILES {
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

struct Tile {
    y: u32,
    height: u32,
    encoded_png: String,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    tiles: Vec<Tile>,
    generation: u64,
}

impl Frame {
    /// Encodes bounded horizontal bands without copying the raster into tiles.
    pub fn from_pixels(pixels: tiny_skia::Pixmap) -> Result<Self> {
        let (width, height) = (pixels.width(), pixels.height());
        if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
            bail!("viewport image exceeds the {MAX_IMAGE_DIMENSION}-pixel graphics limit");
        }
        let tile_height = (MAX_IMAGE_PIXELS / width).min(MAX_IMAGE_DIMENSION);
        let mut data = pixels.take();
        // PNG stores straight alpha; tiny-skia renders premultiplied pixels.
        for pixel in data.chunks_exact_mut(4) {
            if pixel[3] != 255 && pixel[3] != 0 {
                let color = tiny_skia::PremultipliedColorU8::from_rgba(
                    pixel[0], pixel[1], pixel[2], pixel[3],
                )
                .unwrap()
                .demultiply();
                pixel.copy_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
            }
        }
        let mut tiles = Vec::with_capacity(height.div_ceil(tile_height) as usize);
        for y in (0..height).step_by(tile_height as usize) {
            let band_height = tile_height.min(height - y);
            let start = y as usize * width as usize * 4;
            let end = start + band_height as usize * width as usize * 4;
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new_with_quality(
                &mut png,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(
                &data[start..end],
                width,
                band_height,
                image::ExtendedColorType::Rgba8,
            )
            .context("encoding viewport image tile")?;
            tiles.push(Tile {
                y,
                height: band_height,
                encoded_png: STANDARD.encode(png),
            });
        }
        Ok(Self {
            width,
            height,
            tiles,
            generation: NEXT_FRAME.fetch_add(2, Ordering::Relaxed),
        })
    }

    /// Places the visible intersections of bounded image bands at native scale.
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
        let previous_tiles = UPLOADED_TILES.load(Ordering::Relaxed);
        let cached = previous & !1 == self.generation;
        let slot = if cached {
            previous & 1
        } else {
            (previous & 1) ^ 1
        };
        if !cached {
            UPLOADED_FRAME.store(0, Ordering::Relaxed);
        }
        for (index, tile) in self.tiles.iter().enumerate() {
            let image_id = 1 + slot + index as u64 * 2;
            let top = crop.y.max(tile.y);
            let bottom = (crop.y + crop.height).min(tile.y + tile.height);
            let visible = top < bottom;
            if visible {
                let destination_y = crop.top + top - crop.y;
                execute!(
                    out,
                    MoveTo(
                        (crop.left / cell_width) as u16,
                        view.top + (destination_y / cell_height) as u16,
                    )
                )?;
                write!(
                    out,
                    "\x1b_Ga={},i={image_id},p=1,x={},y={},w={},h={},X={},Y={},C=1,q=2",
                    if cached { "p" } else { "T" },
                    crop.x,
                    top - tile.y,
                    crop.width,
                    bottom - top,
                    crop.left % cell_width,
                    destination_y % cell_height,
                )?;
            } else if !cached {
                write!(out, "\x1b_Ga=t,i={image_id},q=2")?;
            } else {
                continue;
            }
            if cached {
                out.write_all(b"\x1b\\")?;
            } else {
                let mut chunks = tile
                    .encoded_png
                    .as_bytes()
                    .chunks(PAYLOAD_CHUNK_SIZE)
                    .peekable();
                write!(out, ",t=d,f=100,m={};", u8::from(chunks.len() > 1))?;
                while let Some(chunk) = chunks.next() {
                    out.write_all(chunk)?;
                    out.write_all(b"\x1b\\")?;
                    if chunks.peek().is_some() {
                        write!(out, "\x1b_Gq=2,m={};", u8::from(chunks.len() > 1))?;
                    }
                }
            }
        }
        // Place every replacement before removing any previous placement.
        if !cached && previous != 0 {
            for index in 0..previous_tiles {
                let image_id = 1 + (previous & 1) + u64::from(index) * 2;
                write!(out, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")?;
            }
        } else if cached {
            for (index, tile) in self.tiles.iter().enumerate() {
                if tile.y >= crop.y + crop.height || tile.y + tile.height <= crop.y {
                    let image_id = 1 + slot + index as u64 * 2;
                    write!(out, "\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")?;
                }
            }
        }
        out.flush()?;
        UPLOADED_TILES.store(self.tiles.len() as u32, Ordering::Relaxed);
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
        for index in 0..UPLOADED_TILES.load(Ordering::Relaxed) {
            let image_id = 1 + (uploaded & 1) + u64::from(index) * 2;
            write!(out, "\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")?;
        }
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
        pending: Option<(u64, Option<(u32, u32, u32, u32)>, String)>,
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
                        let placement = (fields.get("a") == Some(&"T")).then_some(area);
                        self.pending = Some((id, placement, String::new()));
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
                        assert!(pixels.width() * pixels.height() <= MAX_IMAGE_PIXELS);
                        self.images.insert(id, pixels);
                        if let Some(area) = area {
                            self.placements.insert(id, area);
                        }
                    }
                }
                if must_remain_visible {
                    assert!(!self.placements.is_empty(), "redraw exposed a blank canvas");
                }
                assert!(
                    self.images.len() <= (2 * MAX_TILES) as usize,
                    "unbounded terminal raster storage"
                );
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
            let frame = Frame::from_pixels(pixels).unwrap();
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

        // The reported document at 250% decodes to more than Herdr's 32 MiB
        // frame limit once base64 encoded. Its transport must be banded even
        // when a large viewport needs the entire raster at once.
        let mut pixels = tiny_skia::Pixmap::new(2000, 3735).unwrap();
        let band_height = MAX_IMAGE_PIXELS / 2000;
        for (row, data) in pixels.data_mut().chunks_exact_mut(2000 * 4).enumerate() {
            for pixel in data.chunks_exact_mut(4) {
                pixel.copy_from_slice(if row < band_height as usize {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let banded = Frame::from_pixels(pixels).unwrap();
        let large_view = Viewport {
            width: 2000,
            height: 2000,
            ..view()
        };
        output.clear();
        banded.draw(&mut output, large_view, 0, 1000).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.images.len(), 2);
        assert_eq!(terminal.placements.len(), 2);
        let mut bands: Vec<_> = terminal.placements.iter().collect();
        bands.sort_by_key(|(id, _)| **id);
        assert_eq!(*bands[0].1, (0, 1000, 2000, band_height - 1000));
        assert_eq!(*bands[1].1, (0, 0, 2000, 3000 - band_height));
        assert_eq!(
            terminal.images[bands[0].0].get_pixel(0, 1000).0,
            [255, 0, 0, 255]
        );
        assert_eq!(
            terminal.images[bands[1].0].get_pixel(0, 0).0,
            [0, 0, 255, 255]
        );
        let uploads = terminal.uploads;
        output.clear();
        banded.draw(&mut output, large_view, 0, 0).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.placements.len(), 1);
        output.clear();
        banded.draw(&mut output, large_view, 0, 1000).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.placements.len(), 2);
        assert_eq!(terminal.uploads, uploads);
        output.clear();
        clear(&mut output).unwrap();
        terminal.apply(&output, false);
        assert!(terminal.placements.is_empty());
        assert_eq!(terminal.images.len(), 2);
        output.clear();
        first.draw(&mut output, view(), 0, 240).unwrap();
        terminal.apply(&output, false);
        assert_eq!(terminal.images.len(), 1);
        assert_eq!(terminal.visible_pixel(), first_pixels.get_pixel(0, 240).0);
        UPLOADED_FRAME.store(0, Ordering::Relaxed);
    }
}
