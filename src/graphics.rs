use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU32, Ordering};

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
static NEXT_FRAME: AtomicU32 = AtomicU32::new(1);

#[derive(Default)]
struct Images {
    // Incomplete uploads remain owned so terminal teardown can remove them.
    uploaded: HashMap<u32, bool>,
    visible: Vec<u32>,
}

thread_local! {
    // Only terminal-output operations touch this registry. Raster/PNG workers
    // construct Frames without accessing terminal state.
    static IMAGES: RefCell<Images> = RefCell::new(Images::default());
}

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
        IMAGES.with_borrow_mut(|images| *images = Images::default());
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
        let images = IMAGES.with_borrow_mut(std::mem::take);
        // Restore input first, even if writing to a disconnected terminal fails.
        let _ = terminal::disable_raw_mode();
        let mut out = io::stdout();
        for image_id in images.uploaded.keys() {
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

/// One independently encoded and reusable document strip.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    encoded_png: String,
    id: u32,
}

impl Frame {
    /// Encodes a single bounded strip, off the terminal-output path.
    pub fn from_pixels(pixels: tiny_skia::Pixmap) -> Result<Self> {
        let (width, height) = (pixels.width(), pixels.height());
        if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
            bail!("strip image exceeds the {MAX_IMAGE_DIMENSION}-pixel graphics limit");
        }
        if u64::from(width) * u64::from(height) > u64::from(MAX_IMAGE_PIXELS) {
            bail!("strip image exceeds the {MAX_IMAGE_PIXELS}-pixel graphics budget");
        }
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
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(&data, width, height, image::ExtendedColorType::Rgba8)
        .context("encoding document strip")?;
        let mut id = NEXT_FRAME.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).context("terminal image IDs exhausted")?;
            match NEXT_FRAME.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => id = current,
            }
        }
        Ok(Self {
            width,
            height,
            encoded_png: STANDARD.encode(png),
            id,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Uploads pixels without changing any placements. Returns false on reuse.
    pub fn upload(&self, out: &mut impl Write) -> Result<bool> {
        let resident = IMAGES.with_borrow(|images| images.uploaded.get(&self.id) == Some(&true));
        if resident {
            return Ok(false);
        }
        IMAGES.with_borrow_mut(|images| images.uploaded.insert(self.id, false));
        let mut chunks = self
            .encoded_png
            .as_bytes()
            .chunks(PAYLOAD_CHUNK_SIZE)
            .peekable();
        write!(
            out,
            "\x1b_Ga=t,i={},q=2,t=d,f=100,m={};",
            self.id,
            u8::from(chunks.len() > 1),
        )?;
        while let Some(chunk) = chunks.next() {
            out.write_all(chunk)?;
            out.write_all(b"\x1b\\")?;
            if chunks.peek().is_some() {
                write!(out, "\x1b_Gq=2,m={};", u8::from(chunks.len() > 1))?;
            }
        }
        IMAGES.with_borrow_mut(|images| images.uploaded.insert(self.id, true));
        Ok(true)
    }

    /// Places a strip intersection at native scale inside the drawable viewport.
    /// Uploads must be explicit; this path never encodes or transmits pixels.
    pub fn place(
        &self,
        out: &mut impl Write,
        view: Viewport,
        source_y: u32,
        height: u32,
        destination_y: u32,
        left: u32,
    ) -> Result<()> {
        if view.columns == 0
            || view.rows <= view.top.saturating_add(1)
            || view.width == 0
            || view.height == 0
        {
            bail!("cannot draw into an empty viewport");
        }
        let height = height
            .min(self.height.saturating_sub(source_y))
            .min(view.height.saturating_sub(destination_y));
        let width = self.width.min(view.width.saturating_sub(left));
        if width == 0 || height == 0 {
            return Ok(());
        }
        if !IMAGES.with_borrow(|images| images.uploaded.get(&self.id) == Some(&true)) {
            bail!("document strip must be uploaded before placement");
        }
        let cell_width = (view.width / u32::from(view.columns)).max(1);
        let cell_height = (view.height / u32::from(view.rows - view.top - 1)).max(1);
        execute!(
            out,
            MoveTo(
                (left / cell_width) as u16,
                view.top + (destination_y / cell_height) as u16,
            )
        )?;
        write!(
            out,
            "\x1b_Ga=p,i={},p=1,x=0,y={source_y},w={width},h={height},X={},Y={},C=1,q=2\x1b\\",
            self.id,
            left % cell_width,
            destination_y % cell_height,
        )?;
        IMAGES.with_borrow_mut(|images| {
            if !images.visible.contains(&self.id) {
                images.visible.push(self.id);
            }
        });
        Ok(())
    }

    /// Explicitly evicts only this strip. Dropping a Frame never writes output.
    pub fn release(&self, out: &mut impl Write) -> io::Result<()> {
        if !IMAGES.with_borrow(|images| images.uploaded.contains_key(&self.id)) {
            return Ok(());
        }
        write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", self.id)?;
        IMAGES.with_borrow_mut(|images| {
            images.uploaded.remove(&self.id);
            images.visible.retain(|&id| id != self.id);
        });
        Ok(())
    }
}

/// Removes old placements only after the caller has placed every replacement.
/// Uploaded strip pixels stay resident for scrolling in either direction.
pub fn finish_frame(out: &mut impl Write, visible: &[u32]) -> io::Result<()> {
    let count = IMAGES.with_borrow(|images| images.visible.len());
    for index in 0..count {
        let id = IMAGES.with_borrow(|images| images.visible[index]);
        if !visible.contains(&id) {
            write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")?;
        }
    }
    IMAGES.with_borrow_mut(|images| images.visible.retain(|id| visible.contains(id)));
    Ok(())
}

/// Hides the document for a popup without evicting reusable image pixels.
pub fn clear(out: &mut impl Write) -> io::Result<()> {
    finish_frame(out, &[])?;
    out.flush()
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Placement {
        source: (u32, u32),
        size: (u32, u32),
        cell: (u32, u32),
        offset: (u32, u32),
    }

    #[derive(Default)]
    struct TerminalImages {
        images: HashMap<u32, image::RgbaImage>,
        placements: HashMap<u32, Placement>,
        pending: Option<(u32, String)>,
        cursor: (u32, u32),
        uploads: usize,
        chunks: usize,
    }

    impl TerminalImages {
        fn apply(&mut self, output: &[u8], must_remain_visible: bool) {
            for command in output.split(|byte| *byte == 0x1b) {
                if let Some(cursor) = command
                    .strip_prefix(b"[")
                    .and_then(|cursor| cursor.strip_suffix(b"H"))
                {
                    let cursor = std::str::from_utf8(cursor).unwrap();
                    let (row, column) = cursor.split_once(';').unwrap();
                    self.cursor = (
                        column.parse::<u32>().unwrap() - 1,
                        row.parse::<u32>().unwrap() - 1,
                    );
                    continue;
                }
                let Some(command) = command.strip_prefix(b"_G") else {
                    continue;
                };
                let command = std::str::from_utf8(command).unwrap();
                let (control, payload) = command.split_once(';').unwrap_or((command, ""));
                let fields: HashMap<_, _> = control
                    .split(',')
                    .filter_map(|field| field.split_once('='))
                    .collect();
                let number = |key| fields.get(key).unwrap_or(&"0").parse::<u32>().unwrap();
                let id = number("i");
                match fields.get("a").copied() {
                    Some("t") => {
                        self.images.remove(&id);
                        self.placements.remove(&id);
                        self.pending = Some((id, String::new()));
                        self.uploads += 1;
                    }
                    Some("p") => {
                        let pixels = self.images.get(&id).expect("placing missing pixels");
                        let placement = Placement {
                            source: (number("x"), number("y")),
                            size: (number("w"), number("h")),
                            cell: self.cursor,
                            offset: (number("X"), number("Y")),
                        };
                        assert!(placement.source.0 + placement.size.0 <= pixels.width());
                        assert!(placement.source.1 + placement.size.1 <= pixels.height());
                        self.placements.insert(id, placement);
                    }
                    Some("d") => {
                        assert!(self.pending.is_none(), "deletion aborts a chunked upload");
                        self.placements.remove(&id);
                        if fields.get("d") == Some(&"I") {
                            self.images.remove(&id);
                        }
                    }
                    Some(action) => panic!("unexpected graphics action {action}"),
                    None => {}
                }
                if let Some((_, data)) = &mut self.pending {
                    data.push_str(payload);
                    self.chunks += 1;
                    if number("m") == 0 {
                        let (id, data) = self.pending.take().unwrap();
                        let png = STANDARD.decode(data).unwrap();
                        let pixels = image::load_from_memory(&png).unwrap().to_rgba8();
                        assert!(pixels.width() <= MAX_IMAGE_DIMENSION);
                        assert!(pixels.height() <= MAX_IMAGE_DIMENSION);
                        assert!(
                            u64::from(pixels.width()) * u64::from(pixels.height())
                                <= u64::from(MAX_IMAGE_PIXELS)
                        );
                        self.images.insert(id, pixels);
                    }
                }
                if must_remain_visible {
                    assert!(!self.placements.is_empty(), "redraw exposed a blank canvas");
                }
            }
        }
    }

    fn strip(seed: u32) -> (Frame, image::RgbaImage) {
        let mut pixels = tiny_skia::Pixmap::new(64, 256).unwrap();
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
        let expected = image::RgbaImage::from_raw(64, 256, pixels.data().to_vec()).unwrap();
        (Frame::from_pixels(pixels).unwrap(), expected)
    }

    #[test]
    fn independent_strips_survive_scroll_replacement_eviction_and_popup() {
        let (first, first_pixels) = strip(1);
        let (second, second_pixels) = strip(2);
        let (ahead, _) = strip(3);
        assert_ne!(first.id(), second.id());
        assert_ne!(second.id(), ahead.id());
        let mut terminal = TerminalImages::default();
        let mut output = Vec::new();
        assert!(first.upload(&mut output).unwrap());
        assert!(second.upload(&mut output).unwrap());
        terminal.apply(&output, false);
        assert!(terminal.placements.is_empty());
        assert_eq!(terminal.images.len(), 2);
        assert!(
            terminal.chunks > terminal.uploads,
            "exercise chunk continuation"
        );
        assert_eq!(terminal.images[&first.id()], first_pixels);
        assert_eq!(terminal.images[&second.id()], second_pixels);

        output.clear();
        first.place(&mut output, view(), 13, 243, 0, 13).unwrap();
        second.place(&mut output, view(), 0, 237, 243, 13).unwrap();
        finish_frame(&mut output, &[first.id(), second.id()]).unwrap();
        terminal.apply(&output, false);
        assert_eq!(
            terminal.placements[&first.id()],
            Placement {
                source: (0, 13),
                size: (64, 243),
                cell: (1, 1),
                offset: (5, 0),
            }
        );
        assert_eq!(
            terminal.placements[&second.id()],
            Placement {
                source: (0, 0),
                size: (64, 237),
                cell: (1, 16),
                offset: (5, 3),
            }
        );

        output.clear();
        assert!(!first.upload(&mut output).unwrap());
        assert!(!second.upload(&mut output).unwrap());
        assert!(output.is_empty());
        first.place(&mut output, view(), 30, 226, 0, 13).unwrap();
        second.place(&mut output, view(), 0, 254, 226, 13).unwrap();
        finish_frame(&mut output, &[first.id(), second.id()]).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.uploads, 2);
        assert_eq!(terminal.placements[&first.id()].source.1, 30);
        assert_eq!(terminal.placements[&second.id()].size.1, 254);

        // Ahead uploads preserve the canvas; replacements precede removals.
        output.clear();
        assert!(ahead.upload(&mut output).unwrap());
        second.place(&mut output, view(), 0, 256, 0, 13).unwrap();
        ahead.place(&mut output, view(), 0, 224, 256, 13).unwrap();
        finish_frame(&mut output, &[second.id(), ahead.id()]).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.images.len(), 3);
        assert_eq!(terminal.placements.len(), 2);
        assert!(!terminal.placements.contains_key(&first.id()));

        output.clear();
        assert!(!first.upload(&mut output).unwrap());
        first.place(&mut output, view(), 13, 243, 0, 13).unwrap();
        second.place(&mut output, view(), 0, 237, 243, 13).unwrap();
        finish_frame(&mut output, &[first.id(), second.id()]).unwrap();
        ahead.release(&mut output).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.uploads, 3);
        assert_eq!(terminal.images.len(), 2);
        assert!(terminal.images.contains_key(&first.id()));
        assert!(terminal.images.contains_key(&second.id()));

        output.clear();
        first.release(&mut output).unwrap();
        terminal.apply(&output, true);
        assert_eq!(terminal.images.len(), 1);
        assert!(terminal.images.contains_key(&second.id()));
        output.clear();
        first.release(&mut output).unwrap();
        assert!(output.is_empty(), "release is idempotent");
        clear(&mut output).unwrap();
        terminal.apply(&output, false);
        assert!(terminal.placements.is_empty());
        assert_eq!(terminal.images.len(), 1);
        output.clear();
        assert!(!second.upload(&mut output).unwrap());
        second.place(&mut output, view(), 0, 256, 0, 13).unwrap();
        finish_frame(&mut output, &[second.id()]).unwrap();
        terminal.apply(&output, false);
        assert_eq!(terminal.uploads, 3);
        assert_eq!(terminal.placements.len(), 1);
        second.release(&mut Vec::new()).unwrap();
    }

    #[test]
    fn small_strip_centers_without_scaling_and_preserves_pixel_offsets() {
        let frame = Frame::from_pixels(tiny_skia::Pixmap::new(301, 201).unwrap()).unwrap();
        let mut output = Vec::new();
        frame.upload(&mut output).unwrap();
        frame.place(&mut output, view(), 0, 201, 139, 249).unwrap();
        let mut terminal = TerminalImages::default();
        terminal.apply(&output, false);
        assert_eq!(
            terminal.placements[&frame.id()],
            Placement {
                source: (0, 0),
                size: (301, 201),
                cell: (31, 9),
                offset: (1, 11),
            }
        );
        frame.release(&mut Vec::new()).unwrap();
    }

    #[test]
    fn exact_fit_and_oversized_intersections_never_reach_reserved_rows() {
        let frame = Frame::from_pixels(tiny_skia::Pixmap::new(1200, 600).unwrap()).unwrap();
        let mut output = Vec::new();
        frame.upload(&mut output).unwrap();
        frame
            .place(&mut output, view(), 120, u32::MAX, 0, 0)
            .unwrap();
        let mut terminal = TerminalImages::default();
        terminal.apply(&output, false);
        assert_eq!(
            terminal.placements[&frame.id()],
            Placement {
                source: (0, 120),
                size: (800, 480),
                cell: (0, 1),
                offset: (0, 0),
            }
        );
        output.clear();
        frame
            .place(&mut output, view(), 599, u32::MAX, 479, 799)
            .unwrap();
        terminal.apply(&output, true);
        assert_eq!(
            terminal.placements[&frame.id()],
            Placement {
                source: (0, 599),
                size: (1, 1),
                cell: (99, 30),
                offset: (7, 15),
            }
        );
        output.clear();
        frame.place(&mut output, view(), u32::MAX, 1, 0, 0).unwrap();
        frame.place(&mut output, view(), 0, 1, u32::MAX, 0).unwrap();
        frame.place(&mut output, view(), 0, 1, 0, u32::MAX).unwrap();
        assert!(output.is_empty());
        frame.release(&mut Vec::new()).unwrap();
    }

    #[test]
    fn placement_requires_an_explicit_upload() {
        let (frame, _) = strip(4);
        let mut output = Vec::new();
        assert!(frame.place(&mut output, view(), 0, 256, 0, 0).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn strip_dimensions_and_total_pixel_budget_are_bounded() {
        assert!(
            Frame::from_pixels(tiny_skia::Pixmap::new(MAX_IMAGE_DIMENSION + 1, 1).unwrap())
                .is_err()
        );
        assert!(
            Frame::from_pixels(tiny_skia::Pixmap::new(1, MAX_IMAGE_DIMENSION + 1).unwrap())
                .is_err()
        );
        // The previously accepted whole 250% raster must be split by the caller.
        assert!(Frame::from_pixels(tiny_skia::Pixmap::new(2000, 3735).unwrap()).is_err());
        assert!(Frame::from_pixels(tiny_skia::Pixmap::new(2048, 2049).unwrap()).is_err());
        assert!(Frame::from_pixels(tiny_skia::Pixmap::new(2048, 2048).unwrap()).is_ok());
    }

    #[test]
    fn png_upload_demultiplies_alpha() {
        let mut pixels = tiny_skia::Pixmap::new(1, 1).unwrap();
        pixels.data_mut().copy_from_slice(&[64, 32, 0, 128]);
        let color = tiny_skia::PremultipliedColorU8::from_rgba(64, 32, 0, 128)
            .unwrap()
            .demultiply();
        let frame = Frame::from_pixels(pixels).unwrap();
        let mut output = Vec::new();
        frame.upload(&mut output).unwrap();
        let mut terminal = TerminalImages::default();
        terminal.apply(&output, false);
        assert_eq!(
            terminal.images[&frame.id()].get_pixel(0, 0).0,
            [color.red(), color.green(), color.blue(), color.alpha()]
        );
        frame.release(&mut Vec::new()).unwrap();
    }
}
