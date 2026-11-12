# md-term

Graphical Markdown viewer for Kitty, Ghostty, and WezTerm. Its Tokyo Night Moon chrome, conditional tab bar, status line, and centered pickers follow `pdfterm` and `office-term`. The executable is `mdterm`.

Pandoc parses Markdown; Typst lays out one continuous, auto-height vector canvas. Bounded horizontal strips around the viewport are rasterized for the Kitty graphics protocol—there are no page breaks or page navigation. Headings, tables, lists, code, local images, and inline/display TeX math render as a document rather than terminal text.

## Install

Requires Rust 1.88+, Pandoc with a Typst writer, and Typst on `PATH`. Development was exercised with Pandoc 3.13 and Typst 0.15.1.

```sh
# macOS
brew install pandoc typst
cargo install --path .
```

On Linux, install Pandoc and Typst using their upstream distributions or your package manager, then run the same Cargo command. A Kitty-graphics-capable terminal is required for interactive viewing. Run outside tmux; tmux is rejected explicitly.

Typst uses installed fonts and its bundled Latin/math fonts. Install a CJK font such as Noto Serif CJK SC for Chinese/Japanese text on Linux; macOS also uses Hiragino Mincho ProN when available. Other scripts require fonts covering their characters.

## Usage

```sh
mdterm README.md
mdterm first.md second.md       # separate tabs
mdterm                         # browse current directory
cat README.md | mdterm          # read piped Markdown
mdterm - < README.md            # explicitly select stdin
mdterm notes.md - < README.md   # mix file and stdin tabs
mdterm --light book.md
mdterm --ppi 120 book.md
mdterm --export document.png README.md
cat README.md | mdterm --export stdin.png
```

With no file arguments, piped stdin is read through EOF. Use `-` to select stdin explicitly, at most once. Keyboard controls still use the controlling terminal. Stdin opens as a private snapshot named `stdin`; relative images and Markdown links resolve from the working directory.

`--export` renders the entire continuous document without a terminal and refuses to overwrite an existing file. It requires exactly one input and allocates a full-document bitmap, unlike interactive viewing. `--ppi` accepts 36–576 (default 240, or 250% zoom; 96 PPI is 100%). Dark rendering is the default.

## Controls

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, mouse wheel | Smooth vertical scrolling |
| `h` / `l`, left/right | Pan horizontally |
| Space / PageDown; Backspace / PageUp | Scroll 85% of the viewport forward/backward |
| `S` | Toggle smooth scrolling (enabled by default) |
| `p` | Toggle detailed background render / encode / upload / output profiling |
| `g` / `G`, Home / End | Document top / bottom |
| `:` | Jump to a percentage (0–100) |
| `/` | Search rendered text blocks, case-insensitively |
| `n` / `N` | Next / previous matching text block |
| `t` | Heading outline |
| Enter / `L` | Browse document links |
| `b` / `B` | Back / forward navigation history |
| `+` / `-` / `0` | Zoom in / out / reset to startup resolution |
| `m` | Toggle fit-width / original resolution |
| `i` | Toggle dark/light rendering |
| `r` / `R` | Reload the active document |
| `f` | Browse for another document in a new tab |
| `o` | Enter a file path to open in a new tab |
| Tab / Shift-Tab; Alt-1…9 | Switch tabs |
| `?` | Help |
| `q` | Close a pane or the active tab; exit after the last tab |
| Esc | Close a pane, clear an active search, otherwise exit |
| Ctrl-C | Exit |

In lists, `j`/`k` or arrows select, Enter activates, and `/` starts a case-insensitive filter. `o` enters a file path. Hidden files/directories are omitted from the browser; paths can still be opened explicitly. The browser recognizes `.md`, `.markdown`, `.mdown`, `.mkd`, and `.mdx` (MDX is treated as Markdown, not executed).

The active file is checked for changes every 750 ms. Stdin is not watched; manual reloads and theme changes reuse its captured contents. Failed reloads retain the previous rendering; edit again or press `r` to retry. A successful reload closes an open outline to discard stale heading selections. Theme changes and reloads re-typeset synchronously; zoom resamples the cached vector canvas without rerunning Pandoc or Typst. Inactive file tabs reload when activated.

Smooth scrolling follows `pdfterm`'s 16 ms eased steps, with immediate reversal and no queued animation backlog. Jumps, search, tab changes, and modal panes cancel unfinished movement. A single background worker rasterizes and encodes reusable strips, targeting 256 logical rows and 1,048,576 pixels per image. Cell-sized raster bleed and transparent edge padding let adjacent strips occupy exclusive whole-cell rectangles without scaling or overlap, including through Herdr. Scroll positions remain pixel-exact. Each image stays within the 4,194,304-pixel transport limit (16 MiB decoded RGBA).

The bounded cache normally keeps the visible area, two viewports ahead, and one behind; crossing a strip boundary reuses existing images instead of rebuilding the viewport. Uploads are interleaved with input outside synchronized drawing. Cold jumps retain the previous complete view until the replacement is ready; obsolete queued work is discarded.

The status line shows main-thread frame work and a `cache` indicator when the visible strips are ready. Press `p` for the latest background strip's SVG rendering and PNG/base64 encoding times, plus main-thread upload and terminal-output/flush timings. Background work is not charged to frame time. These measurements describe application work, not terminal-emulator display latency.

## Math

Use `$E = mc^2$` for inline math and `$$...$$` on separate lines for display math. Fractions, Greek letters, integrals, matrices, and other supported TeX expressions are converted by Pandoc to Typst math. This is not a full LaTeX engine: custom packages/macros and unsupported commands follow Pandoc's conversion limitations.

## Rendering boundaries

- Search, outline, and heading-fragment navigation jump to text-block coordinates; matches are not highlighted.
- Zoom changes raster resolution, not document layout. Keyboard zoom and fit-width share the 36–576 PPI range (up to 600%).
- Local images are copied into a private rendering directory. Relative paths use the source file's directory, or the working directory for stdin. Remote/data images are not fetched and become descriptive text.
- Local Markdown links (including heading fragments) open in new tabs. Heading fragments within the current document stay in that tab. Explicitly selected HTTP(S)/mailto links use the system opener. Other schemes and non-Markdown file links are rejected.
- Source metadata, raw HTML, and styling attributes are not used as executable layout instructions. Raw Typst/TeX blocks are displayed as code. Unsupported math follows Pandoc's conversion behavior. Code blocks are monospaced without syntax highlighting.
- Private rendering files are removed after layout; the cached vector document stays in memory. A stdin snapshot is removed when its tab closes and pending rendering releases it. Exported PNGs are retained.
- Terminal pixel dimensions come from the terminal window size; when unavailable, the viewer assumes 8×16-pixel cells.
- Drawable terminal heights and cell widths above 10,000 pixels, or individual cells above 4,194,304 pixels, exceed the graphics transport limits and are rejected.
