# md-term

Graphical Markdown viewer for Kitty, Ghostty, and WezTerm. Its Tokyo Night Moon chrome, conditional tab bar, status line, and centered pickers follow `pdfterm` and `office-term`. The executable is `mdterm`.

Pandoc parses Markdown; Typst lays out one continuous, auto-height vector canvas. Only the visible region is rasterized for the Kitty graphics protocol—there are no page breaks or page navigation. Headings, tables, lists, code, local images, and inline/display TeX math render as a document rather than terminal text.

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
mdterm --light book.md
mdterm --ppi 120 book.md
mdterm --export document.png README.md
```

`--export` renders the entire continuous document without a terminal and refuses to overwrite an existing file. It requires exactly one input and allocates a full-document bitmap, unlike interactive viewing. `--ppi` accepts 36–288 (default 96). Dark rendering is the default.

## Controls

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, mouse wheel | Scroll continuously |
| `h` / `l`, left/right | Pan horizontally |
| Space / PageDown; Backspace / PageUp | Scroll one viewport forward/backward |
| `g` / `G`, Home / End | Document top / bottom |
| `:` | Jump to a percentage (0–100) |
| `/` | Search rendered text blocks, case-insensitively |
| `n` / `N` | Next / previous matching text block |
| `t` | Heading outline |
| Enter / `L` | Browse document links |
| `b` / `B` | Back / forward navigation history |
| `+` / `-` / `0` | Zoom in / out / reset |
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

The active file is checked for changes every 750 ms. Failed reloads retain the previous rendering; edit again or press `r` to retry. A successful reload closes an open outline to discard stale heading selections. Theme changes and reloads re-typeset synchronously; zoom resamples the cached vector canvas without rerunning Pandoc or Typst. Inactive tabs reload when activated.

## Math

Use `$E = mc^2$` for inline math and `$$...$$` on separate lines for display math. Fractions, Greek letters, integrals, matrices, and other supported TeX expressions are converted by Pandoc to Typst math. This is not a full LaTeX engine: custom packages/macros and unsupported commands follow Pandoc's conversion limitations.

## Rendering boundaries

- Search, outline, and heading-fragment navigation jump to text-block coordinates; matches are not highlighted.
- Zoom changes raster resolution, not document layout. Fit-width is bounded by the 36–288 PPI range.
- Local image paths resolve relative to the Markdown file and are copied into a private rendering directory. Remote/data images are not fetched and become descriptive text.
- Local Markdown links (including heading fragments) open in new tabs. Heading fragments within the current document stay in that tab. Explicitly selected HTTP(S)/mailto links use the system opener. Other schemes and non-Markdown file links are rejected.
- Source metadata, raw HTML, and styling attributes are not used as executable layout instructions. Raw Typst/TeX blocks are displayed as code. Unsupported math follows Pandoc's conversion behavior. Code blocks are monospaced without syntax highlighting.
- Private rendering files are removed after layout; the cached vector document stays in memory. Exported PNGs are retained.
- Terminal pixel dimensions come from the terminal window size; when unavailable, the viewer assumes 8×16-pixel cells.
