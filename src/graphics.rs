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
const IMAGE_ID: u32 = 1;
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);
// Only this generation owns IMAGE_ID. Switching frames replaces its pixel data.
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
    Ok(Viewport {
        columns,
        rows,
        top,
        width: u32::from(columns) * cell_width,
        height: u32::from(rows - top - 1) * cell_height,
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
        let _ = write!(out, "\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\");
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
        Ok(Self {
            width,
            height,
            encoded_png: STANDARD.encode(png),
            generation: NEXT_FRAME.fetch_add(1, Ordering::Relaxed),
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
        placements_clear(out)?;
        if UPLOADED_FRAME.load(Ordering::Relaxed) != self.generation {
            // Replace the previous raster: terminal storage stays at one viewport.
            UPLOADED_FRAME.store(0, Ordering::Relaxed);
            write!(out, "\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\")?;
            let chunks = self.encoded_png.as_bytes().chunks(PAYLOAD_CHUNK_SIZE);
            let count = chunks.len();
            for (index, chunk) in chunks.enumerate() {
                let more = u8::from(index + 1 < count);
                if index == 0 {
                    write!(out, "\x1b_Ga=t,t=d,f=100,i={IMAGE_ID},q=2,m={more};")?;
                } else {
                    write!(out, "\x1b_Gq=2,m={more};")?;
                }
                out.write_all(chunk)?;
                out.write_all(b"\x1b\\")?;
            }
            out.flush()?;
            UPLOADED_FRAME.store(self.generation, Ordering::Relaxed);
        }
        execute!(
            out,
            MoveTo(
                (crop.left / cell_width) as u16,
                view.top + (crop.top / cell_height) as u16
            )
        )?;
        write!(
            out,
            "\x1b_Ga=p,i={IMAGE_ID},p=1,x={},y={},w={},h={},X={},Y={},C=1,q=2\x1b\\",
            crop.x,
            crop.y,
            crop.width,
            crop.height,
            crop.left % cell_width,
            crop.top % cell_height,
        )?;
        out.flush()?;
        Ok(())
    }
}

/// Removes placements only; cached image pixels remain available to Frame::draw.
pub fn clear(out: &mut impl Write) -> io::Result<()> {
    placements_clear(out)
}

pub fn placements_clear(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b_Ga=d,d=i,i={IMAGE_ID},q=2\x1b\\")?;
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
}
