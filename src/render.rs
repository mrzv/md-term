use anyhow::{Context, Result, bail, ensure};
use resvg::{tiny_skia, usvg};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct RenderOptions {
    pub dark: bool,
    pub ppi: u32,
}

pub struct Entry {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub level: Option<u8>,
    pub target: Option<String>,
    pub id: Option<String>,
}

pub struct Document {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    tree: usvg::Tree,
}

pub struct Raster {
    pub pixels: tiny_skia::Pixmap,
    pub render_elapsed: Duration,
}

impl Document {
    pub fn dimensions(&self, ppi: u32) -> (u32, u32) {
        let scale = f64::from(ppi) / 72.0;
        (
            (f64::from(self.tree.size().width()) * scale).ceil() as u32,
            (f64::from(self.tree.size().height()) * scale).ceil() as u32,
        )
    }

    pub fn rasterize(&self, ppi: u32, x: u32, y: u32, width: u32, height: u32) -> Result<Raster> {
        ensure!(ppi > 0, "render resolution must be greater than zero");
        let (canvas_width, canvas_height) = self.dimensions(ppi);
        ensure!(
            width > 0
                && height > 0
                && u64::from(x) + u64::from(width) <= u64::from(canvas_width)
                && u64::from(y) + u64::from(height) <= u64::from(canvas_height),
            "raster region is outside the document canvas"
        );
        let render_started = Instant::now();
        let mut pixels =
            tiny_skia::Pixmap::new(width, height).context("cannot allocate document viewport")?;
        let scale = ppi as f32 / 72.0;
        let transform =
            tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, -(x as f32), -(y as f32));
        resvg::render(&self.tree, transform, &mut pixels.as_mut());
        let render_elapsed = render_started.elapsed();
        Ok(Raster {
            pixels,
            render_elapsed,
        })
    }
}

// No document-supplied template, filter, defaults, or metadata reaches either writer.
// Raw attributes are accepted by the reader only so they can be displayed as inert code.
const READER: &str = "gfm+attributes+definition_lists+raw_attribute+tex_math_dollars-raw_html";

const PREAMBLE: &str = r##"
#set page(width: 600pt, height: auto, margin: 24pt, fill: md-background)
#set text(size: 11pt, fill: md-foreground, font: (
  "Libertinus Serif", "Noto Serif CJK SC", "Hiragino Mincho ProN", "Arial Unicode MS",
))
#set par(justify: false, leading: 0.65em)
#set heading(numbering: none)
#set smartquote(enabled: false)
#set terms(hanging-indent: 1.5em)
#set table(inset: 5pt, stroke: 0.4pt + md-muted)
#show raw.where(block: true): it => block(
  width: 100%, inset: 9pt, radius: 3pt, fill: md-code-background, it,
)
#show quote.where(block: true): it => block(
  inset: (left: 10pt), stroke: (left: 2pt + md-muted), it,
)
#let divider() = line(length: 100%, stroke: 0.5pt + md-muted)
#let md-entry(text, level, target, id) = context {
  let position = here().position()
  [#metadata((
    text: text, x: position.x / 1pt, y: position.y / 1pt,
    level: level, target: target, id: id,
  )) <mdterm-entry>]
}
#let md-code(text) = {
  show raw.where(block: true): it => context {
    let position = it.location().position()
    [
      #metadata((
        text: text, x: position.x / 1pt, y: position.y / 1pt,
        level: none, target: none, id: none,
      )) <mdterm-entry>
      #it
    ]
  }
  raw(text, block: true)
}
"##;

pub fn render(path: &Path, options: RenderOptions) -> Result<Document> {
    ensure!(
        options.ppi > 0,
        "render resolution must be greater than zero"
    );
    let path = fs::canonicalize(path)
        .with_context(|| format!("cannot open Markdown document {}", path.display()))?;
    ensure!(path.is_file(), "not a Markdown file: {}", path.display());
    let work = tempfile::Builder::new()
        .prefix("md-term-")
        .tempdir()
        .context("cannot create private rendering directory")?;

    let output = run(
        Command::new("pandoc")
            .arg("--from")
            .arg(READER)
            .arg("--to=json")
            .arg("--")
            .arg(&path),
        "reading Markdown with Pandoc",
    )?;
    let mut document: Value =
        serde_json::from_slice(&output).context("Pandoc returned invalid document JSON")?;
    document["meta"] = json!({});
    let blocks = document
        .get_mut("blocks")
        .context("Pandoc document has no blocks")?;
    let mut sanitizer = Sanitizer {
        base: path.parent().context("document has no parent directory")?,
        work: work.path(),
        images: HashMap::new(),
    };
    sanitizer.walk(blocks)?;
    if blocks.as_array().is_some_and(Vec::is_empty) {
        *blocks = json!([raw_block("#box(width: 0pt, height: 0pt)")]);
    }
    let ast_path = work.path().join("document.json");
    fs::write(&ast_path, serde_json::to_vec(&document)?)
        .context("cannot write sanitized Pandoc document")?;
    let body = run(
        Command::new("pandoc")
            .arg("--from=json")
            .arg("--to=typst")
            .arg("--no-highlight")
            .arg("--wrap=none")
            .arg("--")
            .arg(&ast_path),
        "converting sanitized Markdown to Typst",
    )?;
    let body = String::from_utf8(body).context("Pandoc emitted non-UTF-8 Typst")?;
    let palette = if options.dark {
        ["#1e2030", "#c8d3f5", "#222436", "#636da6", "#82aaff"]
    } else {
        ["#ffffff", "#24292f", "#f3f4f6", "#9ca3af", "#0969da"]
    };
    let mut source = String::new();
    for (name, color) in [
        "background",
        "foreground",
        "code-background",
        "muted",
        "link",
    ]
    .into_iter()
    .zip(palette)
    {
        source.push_str(&format!("#let md-{name} = rgb(\"{color}\")\n"));
    }
    source.push_str(PREAMBLE);
    source.push_str(&body);
    let source_path = work.path().join("document.typ");
    fs::write(&source_path, source).context("cannot write generated Typst document")?;
    let svg_path = work.path().join("document.svg");
    run(
        Command::new("typst")
            .arg("compile")
            .arg("--root")
            .arg(work.path())
            .arg(&source_path)
            .arg(&svg_path),
        "laying out document with Typst",
    )?;
    // Typst's SVG coordinates and navigation metadata are both in points.
    let svg_options = usvg::Options {
        dpi: 72.0,
        resources_dir: Some(work.path().to_owned()),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(
        &fs::read(&svg_path).context("cannot read generated document SVG")?,
        &svg_options,
    )
    .context("cannot parse generated document SVG")?;
    let query = run(
        Command::new("typst")
            .arg("query")
            .arg("--root")
            .arg(work.path())
            .arg("--field=value")
            .arg(&source_path)
            .arg("<mdterm-entry>"),
        "locating document search and navigation entries",
    )?;
    let entries = parse_entries(&query)?;
    Ok(Document {
        path,
        entries,
        tree,
    })
}

/// Export never truncates an existing file, including a dangling symlink.
pub fn export(path: &Path, output: &Path, options: RenderOptions) -> Result<()> {
    // Fail cheaply before rendering, while create_new below closes the race.
    match fs::symlink_metadata(output) {
        Ok(_) => bail!("output already exists: {}", output.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect export destination"),
    }
    let ppi = options.ppi;
    let document = render(path, options)?;
    let (width, height) = document.dimensions(ppi);
    let raster = document.rasterize(ppi, 0, 0, width, height)?;
    let png = raster
        .pixels
        .encode_png()
        .context("cannot encode exported PNG")?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("cannot create export {}", output.display()))?;
    let written = (|| -> io::Result<()> {
        destination.write_all(&png)?;
        destination.sync_all()
    })();
    if let Err(error) = written {
        drop(destination);
        let _ = fs::remove_file(output);
        return Err(error).context("cannot write exported PNG");
    }
    Ok(())
}

fn run(command: &mut Command, stage: &str) -> Result<Vec<u8>> {
    let output = command.stdin(Stdio::null()).output().with_context(|| {
        format!(
            "{stage}: cannot start {:?}; install pandoc and typst and ensure they are on PATH",
            command.get_program()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{stage} failed ({}):\n{}", output.status, stderr.trim());
    }
    Ok(output.stdout)
}

struct Sanitizer<'a> {
    base: &'a Path,
    work: &'a Path,
    images: HashMap<PathBuf, String>,
}

impl Sanitizer<'_> {
    fn walk(&mut self, node: &mut Value) -> Result<()> {
        // Strip every attribute class/key-value pair, including table-cell attributes.
        // The Typst writer interprets typst:* values as executable expressions.
        if let Some(array) = node.as_array_mut() {
            if is_attribute(array) {
                array[1] = json!([]);
                array[2] = json!([]);
                return Ok(());
            }
            for child in array {
                self.walk(child)?;
            }
            return Ok(());
        }
        let Some(kind) = node.get("t").and_then(Value::as_str).map(str::to_owned) else {
            return Ok(());
        };
        if kind == "RawInline" || kind == "RawBlock" {
            let text = node["c"][1].as_str().unwrap_or_default().to_owned();
            *node = if kind == "RawInline" {
                json!({"t": "Code", "c": [["", [], []], text]})
            } else {
                json!({"t": "CodeBlock", "c": [["", [], []], text]})
            };
        }
        let kind = node["t"].as_str().unwrap_or_default().to_owned();
        if kind == "DefinitionList" {
            if let Some(items) = node["c"].as_array_mut() {
                for item in items {
                    let text = plain_text(&item[0]);
                    self.walk(item)?;
                    if !text.is_empty() {
                        if let Some(term) = item[0].as_array_mut() {
                            term.insert(0, raw_inline(&marker(&text, None, None, None)));
                        }
                    }
                }
            }
            return Ok(());
        }
        let searchable = matches!(
            kind.as_str(),
            "Plain" | "Para" | "Header" | "Link" | "CodeBlock" | "LineBlock"
        );
        let text = if searchable {
            plain_text(node)
        } else {
            String::new()
        };
        let level = (kind == "Header").then(|| node["c"][0].as_u64()).flatten();
        let id = if kind == "Header" {
            node["c"][1][0]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        } else {
            None
        };
        let target = if kind == "Link" {
            node["c"][2][0].as_str().map(str::to_owned)
        } else {
            None
        };
        if let Some(content) = node.get_mut("c") {
            self.walk(content)?;
        }
        match kind.as_str() {
            "Image" => {
                let url = node["c"][2][0].as_str().unwrap_or_default().to_owned();
                match self.stage_image(&url)? {
                    Some(resource) => node["c"][2][0] = json!(resource),
                    None => {
                        let alt = plain_text(node);
                        *node = json!({"t": "Str", "c": format!("[image: {alt} ({url})]")});
                    }
                }
            }
            "Link" => {
                let mut inlines = node["c"][1].as_array().cloned().unwrap_or_default();
                inlines.insert(
                    0,
                    raw_inline(&format!(
                        "{}#text(fill: md-link)[",
                        marker(&text, None, target.as_deref(), None)
                    )),
                );
                inlines.push(raw_inline("]"));
                *node = json!({"t": "Span", "c": [["", [], []], inlines]});
            }
            "Header" | "Plain" | "Para" => {
                let inlines = if kind == "Header" {
                    &mut node["c"][2]
                } else {
                    &mut node["c"]
                };
                if !text.trim().is_empty() {
                    if let Some(inlines) = inlines.as_array_mut() {
                        inlines.insert(0, raw_inline(&marker(&text, level, None, id.as_deref())));
                    }
                }
            }
            "CodeBlock" => {
                let code = node["c"][1].as_str().unwrap_or_default();
                *node = raw_block(&format!("#md-code({})", typst_string(code)));
            }
            "LineBlock" => {
                if let Some(first) = node["c"]
                    .as_array_mut()
                    .and_then(|lines| lines.first_mut())
                    .and_then(Value::as_array_mut)
                {
                    first.insert(0, raw_inline(&marker(&text, None, None, None)));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stage_image(&mut self, url: &str) -> Result<Option<String>> {
        // Never fetch URLs: layout cannot cause network requests, template imports,
        // or access to paths outside the explicit local image references.
        let local = if let Some(path) = url.strip_prefix("file://localhost/") {
            format!("/{path}")
        } else if let Some(path) = url.strip_prefix("file:///") {
            format!("/{path}")
        } else if has_scheme(url) || url.starts_with("//") {
            return Ok(None);
        } else {
            url.to_owned()
        };
        let local = local.split(['?', '#']).next().unwrap_or_default();
        let decoded =
            percent_decode(local).with_context(|| format!("invalid local image URL {url:?}"))?;
        let path = self.base.join(decoded);
        let path = fs::canonicalize(&path)
            .with_context(|| format!("cannot resolve local image {}", path.display()))?;
        if let Some(staged) = self.images.get(&path) {
            return Ok(Some(staged.clone()));
        }
        ensure!(
            path.is_file(),
            "local image is not a file: {}",
            path.display()
        );
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("image");
        // Generated names, not source names, are passed through the Typst writer.
        let extension: String = extension
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let staged = format!("resource-{}.{}", self.images.len(), extension);
        fs::copy(&path, self.work.join(&staged))
            .with_context(|| format!("cannot stage local image {}", path.display()))?;
        self.images.insert(path, staged.clone());
        Ok(Some(staged))
    }
}

fn is_attribute(array: &[Value]) -> bool {
    array.len() == 3
        && array[0].is_string()
        && array[1]
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
        && array[2].as_array().is_some_and(|items| {
            items.iter().all(|item| {
                item.as_array()
                    .is_some_and(|pair| pair.len() == 2 && pair.iter().all(Value::is_string))
            })
        })
}

fn raw_inline(source: &str) -> Value {
    json!({"t": "RawInline", "c": ["typst", source]})
}

fn raw_block(source: &str) -> Value {
    json!({"t": "RawBlock", "c": ["typst", source]})
}

fn typst_string(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 2);
    result.push('"');
    for ch in text.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            ch if ch.is_control() => result.push_str(&format!("\\u{{{:x}}}", ch as u32)),
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

fn marker(text: &str, level: Option<u64>, target: Option<&str>, id: Option<&str>) -> String {
    format!(
        "#md-entry({}, {}, {}, {})",
        typst_string(text),
        level.map_or_else(|| "none".to_owned(), |level| level.to_string()),
        target.map_or_else(|| "none".to_owned(), typst_string),
        id.map_or_else(|| "none".to_owned(), typst_string),
    )
}

fn plain_text(node: &Value) -> String {
    fn append(node: &Value, out: &mut String) {
        if let Some(items) = node.as_array() {
            for item in items {
                append(item, out);
            }
            return;
        }
        let content = &node["c"];
        match node["t"].as_str().unwrap_or_default() {
            "Str" => out.push_str(content.as_str().unwrap_or_default()),
            "Space" | "SoftBreak" => out.push(' '),
            "LineBreak" => out.push('\n'),
            "Code" | "CodeBlock" | "Math" | "RawInline" | "RawBlock" => {
                out.push_str(content[1].as_str().unwrap_or_default());
            }
            "Header" => append(&content[2], out),
            "Link" | "Image" | "Span" | "Cite" | "Quoted" => append(&content[1], out),
            _ if node.is_object() => append(content, out),
            _ => {}
        }
    }
    let mut text = String::new();
    append(node, &mut text);
    text
}

fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            ensure!(index + 2 < bytes.len(), "incomplete percent escape");
            let high = (bytes[index + 1] as char)
                .to_digit(16)
                .context("invalid percent escape")?;
            let low = (bytes[index + 2] as char)
                .to_digit(16)
                .context("invalid percent escape")?;
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("image URL is not UTF-8")
}

fn parse_entries(query: &[u8]) -> Result<Vec<Entry>> {
    #[derive(Deserialize)]
    struct LocatedEntry {
        text: String,
        x: f64,
        y: f64,
        level: Option<u8>,
        target: Option<String>,
        id: Option<String>,
    }
    let entries: Vec<LocatedEntry> =
        serde_json::from_slice(query).context("Typst returned invalid navigation metadata")?;
    entries
        .into_iter()
        .map(|entry| {
            ensure!(
                entry.x.is_finite() && entry.y.is_finite() && entry.x >= 0.0 && entry.y >= 0.0,
                "Typst navigation entry has invalid coordinates"
            );
            Ok(Entry {
                text: entry.text,
                x: entry.x,
                y: entry.y,
                level: entry.level,
                target: entry.target,
                id: entry.id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typst_strings_cannot_escape_into_code() {
        assert_eq!(
            typst_string("\"\\\n\r\t\0#[]"),
            "\"\\\"\\\\\\n\\r\\t\\u{0}#[]\""
        );
        assert_eq!(typst_string("雪"), "\"雪\"");
    }

    #[test]
    fn navigation_entries_preserve_canvas_coordinates() {
        let entries = parse_entries(br##"[{"text":"heading","x":24.5,"y":9000.25,"level":2,"target":null,"id":"heading"},{"text":"link","x":150,"y":42,"level":null,"target":"#heading","id":null}]"##).unwrap();
        assert_eq!((entries[0].x, entries[0].y), (24.5, 9000.25));
        assert_eq!(entries[0].level, Some(2));
        assert_eq!(entries[0].id.as_deref(), Some("heading"));
        assert_eq!((entries[1].x, entries[1].y), (150.0, 42.0));
        assert_eq!(entries[1].target.as_deref(), Some("#heading"));
        for query in [
            br#"[{"text":"bad","x":-1,"y":0}]"#.as_slice(),
            br#"[{"text":"bad","x":0,"y":-1}]"#.as_slice(),
            br#"[{"text":"bad","x":0,"y":1e999}]"#.as_slice(),
            br#"[{"text":"bad","x":0}]"#.as_slice(),
        ] {
            assert!(parse_entries(query).is_err());
        }
    }

    #[test]
    fn rasterization_scales_points_and_allocates_only_requested_region() {
        let tree = usvg::Tree::from_str(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="72pt" height="144pt" viewBox="0 0 72 144"><rect width="72" height="144" fill="white"/><rect x="18" y="72" width="18" height="18" fill="#ff0000"/></svg>"##,
            &usvg::Options {
                dpi: 72.0,
                ..usvg::Options::default()
            },
        )
        .unwrap();
        let document = Document {
            path: PathBuf::new(),
            entries: Vec::new(),
            tree,
        };
        assert_eq!(document.dimensions(72), (72, 144));
        assert_eq!(document.dimensions(144), (144, 288));
        let raster = document.rasterize(144, 36, 144, 36, 36).unwrap();
        assert_eq!((raster.pixels.width(), raster.pixels.height()), (36, 36));
        assert_eq!(raster.pixels.pixel(18, 18).unwrap().demultiply().red(), 255);
        assert_eq!(raster.pixels.pixel(18, 18).unwrap().demultiply().green(), 0);
        assert_eq!(raster.pixels.pixel(18, 18).unwrap().alpha(), 255);
        assert!(document.rasterize(144, 144, 0, 1, 1).is_err());
        assert!(document.rasterize(0, 0, 0, 1, 1).is_err());
    }

    #[test]
    fn distant_clipped_regions_keep_document_pixel_coordinates() {
        let mut svg = String::from(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="144pt" height="150144pt" viewBox="0 0 144 150144"><defs><clipPath id="page"><rect width="144" height="150144"/></clipPath><clipPath id="tile"><rect width="72" height="72"/></clipPath></defs><g clip-path="url(#page)"><rect width="144" height="150144" fill="white"/>"##,
        );
        for y in [24, 50024, 150024] {
            svg.push_str(&format!(
                r##"<g transform="translate(24 {y})" clip-path="url(#tile)"><rect width="72" height="72" fill="#ff0000"/><path d="M 18 18 H 54 V 54 H 18 Z" fill="#00ff00"/></g>"##,
            ));
        }
        svg.push_str("</g></svg>");
        let tree = usvg::Tree::from_str(
            &svg,
            &usvg::Options {
                dpi: 72.0,
                ..usvg::Options::default()
            },
        )
        .unwrap();
        let document = Document {
            path: PathBuf::new(),
            entries: Vec::new(),
            tree,
        };
        // Visit far-apart regions and then jump back, at both native and
        // startup resolutions. Neither a full-document pixmap nor cache reuse
        // may affect the coordinates passed into resvg's clip transforms.
        for ppi in [72, 240] {
            let scale = ppi / 24;
            for y in [50024, 24, 150024, 50024] {
                let raster = document
                    .rasterize(ppi, 8 * scale, y * scale / 3, 24 * scale, 24 * scale)
                    .unwrap();
                assert_eq!(
                    (raster.pixels.width(), raster.pixels.height()),
                    (24 * scale, 24 * scale)
                );
                let red = raster.pixels.pixel(scale, scale).unwrap().demultiply();
                let green = raster
                    .pixels
                    .pixel(12 * scale, 12 * scale)
                    .unwrap()
                    .demultiply();
                assert_eq!(
                    [red.red(), red.green(), red.blue(), red.alpha()],
                    [255, 0, 0, 255]
                );
                assert_eq!(
                    [green.red(), green.green(), green.blue(), green.alpha()],
                    [0, 255, 0, 255]
                );
            }
        }
    }

    #[test]
    fn writer_attributes_cannot_inject_typst_code() {
        let work = tempfile::tempdir().unwrap();
        let mut sanitizer = Sanitizer {
            base: work.path(),
            work: work.path(),
            images: HashMap::new(),
        };
        let mut ast = json!([
            {"t":"Header", "c":[1,["anchor",["typst:no-figure"],[["typst:fill", "panic(\"injected\")"]]],[{"t":"Str","c":"Title"}]]},
            {"t":"Span", "c":[["",[],[["typst:text:fill","panic(\"injected\")"]]],[{"t":"Str","c":"safe"}]]}
        ]);
        sanitizer.walk(&mut ast).unwrap();
        assert_eq!(ast[0]["c"][1], json!(["anchor", [], []]));
        assert_eq!(ast[1]["c"][0], json!(["", [], []]));
    }

    #[test]
    fn images_resolve_from_document_directory_and_are_staged_once() {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(base.path().join("雪 #1.png"), b"image bytes").unwrap();
        let mut sanitizer = Sanitizer {
            base: base.path(),
            work: work.path(),
            images: HashMap::new(),
        };
        let first = sanitizer
            .stage_image("%E9%9B%AA%20%231.png")
            .unwrap()
            .unwrap();
        let second = sanitizer.stage_image("雪%20%231.png").unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(work.path().join(first)).unwrap(), b"image bytes");
        assert!(
            sanitizer
                .stage_image("https://example.com/image.png")
                .unwrap()
                .is_none()
        );
        assert!(
            sanitizer
                .stage_image("//example.com/image.png")
                .unwrap()
                .is_none()
        );
        assert!(percent_decode("%xy").is_err());
    }
}
