use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind},
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use unicode_width::UnicodeWidthChar;

use crate::{
    graphics::{self, TerminalGuard, Viewport},
    render::{self, Document, RenderOptions, Source},
};

#[path = "raster_cache.rs"]
mod raster_cache;
use raster_cache::StripCache;

static NEXT_DOCUMENT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Position {
    x: u32,
    y: u32,
}

const SCROLL_FRAME: Duration = Duration::from_millis(16);

struct SmoothScroll {
    enabled: bool,
    target: Option<u32>,
    tick: Instant,
}

impl SmoothScroll {
    fn new(now: Instant) -> Self {
        Self {
            enabled: true,
            target: None,
            tick: now,
        }
    }

    fn cancel(&mut self) {
        self.target = None;
    }

    fn scroll(&mut self, position: &mut u32, delta: i64, maximum: u32, now: Instant) -> bool {
        let before = *position;
        *position = (*position).min(maximum);
        let mut target = self.target.unwrap_or(*position).min(maximum);
        let remaining = i64::from(target) - i64::from(*position);
        // Reversing starts from the displayed position, not an unfinished animation.
        if remaining.signum() != delta.signum() {
            target = *position;
        }
        target = i64::from(target)
            .saturating_add(delta)
            .clamp(0, i64::from(maximum)) as u32;
        if self.enabled {
            if self.target.is_none() {
                self.tick = now;
            }
            self.target = (target != *position).then_some(target);
        } else {
            *position = target;
            self.cancel();
        }
        before != *position
    }

    fn next_position(&self, position: u32) -> Option<u32> {
        let target = self.target?;
        let remaining = i64::from(target) - i64::from(position);
        let step = remaining.unsigned_abs().div_ceil(4) as i64 * remaining.signum();
        Some((i64::from(position) + step) as u32)
    }

    fn advance(&mut self, position: &mut u32, now: Instant) -> bool {
        let Some(target) = self.target else {
            return false;
        };
        if now.duration_since(self.tick) < SCROLL_FRAME {
            return false;
        }
        self.tick = now;
        let next = self.next_position(*position).unwrap();
        let changed = next != *position;
        *position = next;
        if *position == target {
            self.cancel();
        }
        changed
    }

    fn wait(&self, now: Instant) -> Duration {
        let background = Duration::from_millis(100);
        if self.target.is_some() {
            SCROLL_FRAME
                .saturating_sub(now.duration_since(self.tick))
                .min(background)
        } else {
            background
        }
    }

    fn toggle(&mut self, position: &mut u32) {
        self.enabled = !self.enabled;
        if !self.enabled {
            if let Some(target) = self.target.take() {
                *position = target;
            }
        }
    }
}

fn zoom_percent(ppi: u32) -> u32 {
    (f64::from(ppi) * 100.0 / 96.0).round() as u32
}

enum Wake {
    Input(io::Result<Event>),
    Raster,
}

// Crossterm's blocking reader and the raster worker share one wakeable event queue.
// Only this reader touches Crossterm input; shutting down never leaves a reader behind.
struct InputReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl InputReader {
    fn start(sender: Sender<Wake>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("mdterm-input".into())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    match event::poll(Duration::from_millis(100)) {
                        Ok(false) => {}
                        Ok(true) => {
                            let event = event::read();
                            let failed = event.is_err();
                            if sender.send(Wake::Input(event)).is_err() || failed {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Wake::Input(Err(error)));
                            break;
                        }
                    }
                }
            })
            .context("cannot start terminal input reader")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Performance {
    render: Duration,
    encode: Duration,
    upload: Duration,
    blocking: Duration,
    output: Duration,
    cache_hit: bool,
}

// Keep placement output inside the enclosing synchronized update.
struct FrameOutput<'a, W>(&'a mut W);

impl<W: Write> Write for FrameOutput<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Tab {
    document: Arc<Document>,
    generation: u64,
    position: Position,
    back: Vec<Position>,
    forward: Vec<Position>,
    options: RenderOptions,
    base_ppi: u32,
    fit_width: bool,
    stamp: Option<(SystemTime, u64)>,
    query: String,
    matches: Vec<usize>,
    match_index: usize,
}

impl Tab {
    fn open(source: Arc<Source>, options: RenderOptions) -> Result<Self> {
        // Associate the rendering with the version Pandoc is about to read.
        let stamp = stamp(&source);
        let document = render::render(
            source,
            RenderOptions {
                dark: options.dark,
                ppi: options.ppi,
            },
        )?;
        Ok(Self {
            document: Arc::new(document),
            generation: NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed),
            position: Position::default(),
            back: vec![],
            forward: vec![],
            base_ppi: options.ppi,
            options,
            fit_width: false,
            stamp,
            query: String::new(),
            matches: vec![],
            match_index: 0,
        })
    }

    fn navigate(&mut self, next: Position) {
        if next != self.position {
            self.back.push(self.position);
            self.forward.clear();
            self.position = next;
        }
    }

    fn entry_position(&self, index: usize) -> Position {
        let entry = &self.document.entries[index];
        let scale = f64::from(self.options.ppi) / 72.0;
        Position {
            x: (entry.x * scale).round().max(0.0) as u32,
            y: (entry.y * scale).round().max(0.0) as u32,
        }
    }

    fn jump_entry(&mut self, index: usize) {
        self.navigate(self.entry_position(index));
    }

    fn history(&mut self, forward: bool) {
        let (from, to) = if forward {
            (&mut self.forward, &mut self.back)
        } else {
            (&mut self.back, &mut self.forward)
        };
        if let Some(position) = from.pop() {
            to.push(self.position);
            self.position = position;
        }
    }

    fn search(&mut self, query: String) {
        self.query = query;
        let needle = self.query.to_lowercase();
        self.matches = if needle.is_empty() {
            vec![]
        } else {
            self.document
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry.target.is_none() && entry.text.to_lowercase().contains(&needle)
                })
                .map(|(index, _)| index)
                .collect()
        };
        self.match_index = 0;
    }

    fn next_match(&mut self, backwards: bool) -> bool {
        if self.matches.is_empty() {
            return false;
        }
        self.match_index = if backwards {
            (self.match_index + self.matches.len() - 1) % self.matches.len()
        } else {
            (self.match_index + 1) % self.matches.len()
        };
        self.jump_entry(self.matches[self.match_index]);
        true
    }

    fn rebuild(&mut self, dark: bool) -> Result<()> {
        let source_stamp = stamp(&self.document.source);
        let document = render::render(
            Arc::clone(&self.document.source),
            RenderOptions {
                dark,
                ppi: self.options.ppi,
            },
        )?;
        // Do not replace the usable document or its options on a failed render.
        self.stamp = source_stamp;
        self.document = Arc::new(document);
        self.generation = NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed);
        self.options.dark = dark;
        self.search(self.query.clone());
        Ok(())
    }

    fn zoom(&mut self, ppi: u32) {
        if ppi == self.options.ppi {
            return;
        }
        let ratio = f64::from(ppi) / f64::from(self.options.ppi);
        for position in std::iter::once(&mut self.position)
            .chain(self.back.iter_mut())
            .chain(self.forward.iter_mut())
        {
            position.x = (f64::from(position.x) * ratio).round() as u32;
            position.y = (f64::from(position.y) * ratio).round() as u32;
        }
        self.options.ppi = ppi;
        self.generation = NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed);
    }

    fn fit(&mut self, view: Viewport) {
        if self.fit_width {
            let (width, _) = self.document.dimensions(self.options.ppi);
            let ppi = (f64::from(self.options.ppi) * f64::from(view.width)
                / f64::from(width.max(1)))
            .floor()
            .clamp(36.0, 288.0) as u32;
            self.zoom(ppi);
        }
        self.clamp(view);
    }

    fn clamp(&mut self, view: Viewport) {
        let (width, height) = self.document.dimensions(self.options.ppi);
        self.position.x = self.position.x.min(width.saturating_sub(view.width));
        self.position.y = self.position.y.min(height.saturating_sub(view.height));
    }

    fn scroll(&mut self, dx: i64, dy: i64, view: Viewport) {
        self.position.x = self
            .position
            .x
            .saturating_add_signed(dx.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        self.position.y = self
            .position
            .y
            .saturating_add_signed(dy.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        self.clamp(view);
    }

    fn jump_percent(&mut self, percent: u8, view: Viewport) {
        let (_, height) = self.document.dimensions(self.options.ppi);
        self.navigate(Position {
            x: 0,
            y: (u64::from(height.saturating_sub(view.height)) * u64::from(percent) / 100) as u32,
        });
    }

    fn percent(&self, view: Viewport) -> u32 {
        let (_, height) = self.document.dimensions(self.options.ppi);
        let range = height.saturating_sub(view.height);
        if range == 0 {
            100
        } else {
            ((u64::from(self.position.y) * 100 + u64::from(range) / 2) / u64::from(range)).min(100)
                as u32
        }
    }
}

fn stamp(source: &Source) -> Option<(SystemTime, u64)> {
    let Source::File(path) = source else {
        return None;
    };
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[derive(Clone)]
enum Action {
    Entry(usize),
    Link(String),
    File(PathBuf),
    Directory(PathBuf),
}

struct Item {
    label: String,
    action: Action,
}
struct List {
    title: String,
    items: Vec<Item>,
    filter: String,
    editing: bool,
    selected: usize,
}

impl List {
    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.label.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Prompt {
    Search,
    Open,
    Percent,
}
enum Mode {
    Normal,
    Prompt(Prompt, String),
    List(List),
    Help(usize),
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    mode: Mode,
    message: String,
    defaults: RenderOptions,
    quitting: bool,
    scroll: SmoothScroll,
    performance: Option<Performance>,
    show_performance: bool,
    cache: StripCache,
}

impl App {
    fn tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    fn scroll_vertical(&mut self, delta: i64, view: Viewport) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        let (_, height) = tab.document.dimensions(tab.options.ppi);
        self.scroll.scroll(
            &mut tab.position.y,
            delta,
            height.saturating_sub(view.height),
            Instant::now(),
        )
    }

    fn advance_scroll(&mut self, now: Instant) -> bool {
        if !matches!(self.mode, Mode::Normal) {
            self.scroll.cancel();
            return false;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            self.scroll.cancel();
            return false;
        };
        let before = tab.position.y;
        let target = self.scroll.target;
        if !self.scroll.advance(&mut tab.position.y, now) {
            return false;
        }
        if !self.cache.ready_at(tab.position.x, tab.position.y) {
            // Never advance into a hole or reverse from an unseen position.
            // Prefetch covers the next step; completion wakes the event loop.
            tab.position.y = before;
            self.scroll.target = target;
            return false;
        }
        true
    }

    fn open(&mut self, path: &Path) -> Result<()> {
        let options = self.tabs.get(self.active).map_or(
            RenderOptions {
                dark: self.defaults.dark,
                ppi: self.defaults.ppi,
            },
            |tab| RenderOptions {
                dark: tab.options.dark,
                ppi: tab.options.ppi,
            },
        );
        let tab = Tab::open(Arc::new(Source::file(path)?), options)?;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.message.clear();
        Ok(())
    }

    fn list(&mut self, title: String, items: Vec<Item>) {
        self.mode = Mode::List(List {
            title,
            items,
            filter: String::new(),
            editing: false,
            selected: 0,
        });
    }

    fn browse(&mut self, path: &Path) -> Result<()> {
        let path =
            fs::canonicalize(path).with_context(|| format!("cannot browse {}", path.display()))?;
        let mut files = vec![];
        let mut dirs = vec![];
        if let Some(parent) = path.parent() {
            dirs.push(Item {
                label: "../".into(),
                action: Action::Directory(parent.to_owned()),
            });
        }
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file = entry.path();
            if file.is_dir() {
                dirs.push(Item {
                    label: format!("{name}/"),
                    action: Action::Directory(file),
                });
            } else if markdown(&file) {
                files.push(Item {
                    label: name,
                    action: Action::File(file),
                });
            }
        }
        dirs.sort_by(|a, b| a.label.cmp(&b.label));
        files.sort_by(|a, b| a.label.cmp(&b.label));
        dirs.extend(files);
        self.list(
            format!("Open {} — / filter, o enter path", path.display()),
            dirs,
        );
        Ok(())
    }

    fn follow(&mut self, target: &str) -> Result<()> {
        let Some(tab) = self.tab() else {
            return Ok(());
        };
        if let Some(fragment) = target.strip_prefix('#') {
            let fragment = decode(fragment)?;
            let index = tab
                .document
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| {
                    entry.level.is_some() && entry.id.as_deref() == Some(fragment.as_str())
                })
                .map(|(index, _)| index)
                .with_context(|| format!("heading not found: #{fragment}"))?;
            tab.jump_entry(index);
            return Ok(());
        }
        if target.starts_with("https://")
            || target.starts_with("http://")
            || target.starts_with("mailto:")
        {
            #[cfg(target_os = "macos")]
            let mut command = Command::new("open");
            #[cfg(not(target_os = "macos"))]
            let mut command = Command::new("xdg-open");
            let result = command
                .arg(target)
                .output()
                .context("cannot launch link opener")?;
            if !result.status.success() {
                bail!(
                    "link opener failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            self.message = format!("Opened {target}");
            return Ok(());
        }
        let (file, fragment) = target
            .split_once('#')
            .map_or((target, None), |(f, a)| (f, Some(a)));
        if file.contains(':') || file.starts_with("//") {
            bail!("unsupported link scheme: {target}");
        }
        let path = tab.document.source.directory().join(decode(file)?);
        if !markdown(&path) {
            bail!("only Markdown files and http(s)/mailto links can be opened");
        }
        self.open(&path)?;
        if let Some(fragment) = fragment {
            self.follow(&format!("#{fragment}"))?;
        }
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Entry(index) => {
                if let Some(tab) = self.tab() {
                    tab.jump_entry(index);
                }
            }
            Action::Link(target) => self.follow(&target)?,
            Action::File(path) => self.open(&path)?,
            Action::Directory(path) => self.browse(&path)?,
        }
        Ok(())
    }

    fn key(&mut self, key: KeyEvent, view: Viewport) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quitting = true;
            return Ok(false);
        }
        if key.modifiers == KeyModifiers::ALT {
            if let KeyCode::Char(c @ '1'..='9') = key.code {
                let index = c as usize - '1' as usize;
                if index < self.tabs.len() {
                    self.scroll.cancel();
                    self.active = index;
                    self.mode = Mode::Normal;
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Ok(false);
        }
        let had_message = !self.message.is_empty();
        self.message.clear();
        if matches!(self.mode, Mode::Normal) && key.code == KeyCode::Char('p') {
            self.show_performance = !self.show_performance;
            return Ok(true);
        }
        if matches!(self.mode, Mode::Normal) {
            let small = i64::from((view.height / 20).max(1));
            let large = (u64::from(view.height) * 85 / 100).max(1) as i64;
            let delta = match key.code {
                KeyCode::Char('j') | KeyCode::Down => Some(small),
                KeyCode::Char('k') | KeyCode::Up => Some(-small),
                KeyCode::Char(' ') | KeyCode::PageDown => Some(large),
                KeyCode::Backspace | KeyCode::PageUp => Some(-large),
                _ => None,
            };
            if let Some(delta) = delta {
                return Ok(self.scroll_vertical(delta, view) || had_message);
            }
            if key.code == KeyCode::Char('S') {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    self.scroll.toggle(&mut tab.position.y);
                } else {
                    self.scroll.enabled = !self.scroll.enabled;
                }
                self.message = if self.scroll.enabled {
                    "Smooth scrolling on"
                } else {
                    "Smooth scrolling off"
                }
                .into();
                return Ok(true);
            }
            if matches!(
                key.code,
                KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right
            ) {
                let Some(tab) = self.tab() else {
                    return Ok(had_message);
                };
                let before = tab.position;
                let delta = if matches!(key.code, KeyCode::Char('h') | KeyCode::Left) {
                    -small
                } else {
                    small
                };
                tab.scroll(delta, 0, view);
                return Ok(tab.position != before || had_message);
            }
            if !matches!(
                key.code,
                KeyCode::Char(
                    'q' | 'f'
                        | 'o'
                        | '/'
                        | ':'
                        | '?'
                        | 't'
                        | 'L'
                        | 'g'
                        | 'G'
                        | 'b'
                        | 'B'
                        | 'n'
                        | 'N'
                        | 'r'
                        | 'R'
                        | 'i'
                        | '+'
                        | '='
                        | '-'
                        | '0'
                        | 'm'
                ) | KeyCode::Esc
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Enter
                    | KeyCode::Home
                    | KeyCode::End
            ) {
                return Ok(had_message);
            }
        }
        self.scroll.cancel();
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Prompt(prompt, mut text) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match prompt {
                    Prompt::Open => self.open(Path::new(&text))?,
                    Prompt::Percent => {
                        let percent: u8 =
                            text.parse().context("enter a percentage from 0 to 100")?;
                        if percent > 100 {
                            bail!("percentage must be between 0 and 100");
                        }
                        self.tab()
                            .context("no document open")?
                            .jump_percent(percent, view);
                    }
                    Prompt::Search => {
                        if let Some(tab) = self.tab() {
                            tab.search(text);
                            if let Some(&index) = tab.matches.first() {
                                tab.jump_entry(index);
                                self.message = format!(
                                    "{} matching blocks; n/N to navigate",
                                    tab.matches.len()
                                );
                            } else {
                                self.message = "No matches".into();
                            }
                        }
                    }
                },
                KeyCode::Backspace => {
                    text.pop();
                    self.mode = Mode::Prompt(prompt, text);
                }
                KeyCode::Char(c) if !c.is_control() => {
                    text.push(c);
                    self.mode = Mode::Prompt(prompt, text);
                }
                _ => self.mode = Mode::Prompt(prompt, text),
            },
            Mode::List(mut list) => {
                let visible = list.visible();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Char('q') if !list.editing => {
                        if self.tabs.is_empty() {
                            self.quitting = true;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(&index) = visible.get(list.selected) {
                            self.action(list.items[index].action.clone())?;
                        } else {
                            self.mode = Mode::List(list);
                        }
                    }
                    KeyCode::Char('o') if !list.editing => {
                        self.mode = Mode::Prompt(Prompt::Open, String::new())
                    }
                    KeyCode::Char('/') if !list.editing => {
                        list.editing = true;
                        self.mode = Mode::List(list);
                    }
                    KeyCode::Backspace if list.editing => {
                        list.filter.pop();
                        list.selected = 0;
                        self.mode = Mode::List(list);
                    }
                    KeyCode::Char(c) if list.editing && !c.is_control() => {
                        list.filter.push(c);
                        list.selected = 0;
                        self.mode = Mode::List(list);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        list.selected = (list.selected + 1).min(visible.len().saturating_sub(1));
                        self.mode = Mode::List(list);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        list.selected = list.selected.saturating_sub(1);
                        self.mode = Mode::List(list);
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        list.selected = 0;
                        self.mode = Mode::List(list);
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        list.selected = visible.len().saturating_sub(1);
                        self.mode = Mode::List(list);
                    }
                    _ => self.mode = Mode::List(list),
                }
            }
            Mode::Help(mut offset) => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    offset = (offset + 1).min(HELP_NAVIGATION.len() + HELP_VIEWER.len() + 3);
                    self.mode = Mode::Help(offset);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mode = Mode::Help(offset.saturating_sub(1));
                }
                KeyCode::Home | KeyCode::Char('g') => self.mode = Mode::Help(0),
                _ => self.mode = Mode::Help(offset),
            },
            Mode::Normal => self.normal(key.code, view)?,
        }
        Ok(true)
    }

    fn normal(&mut self, code: KeyCode, view: Viewport) -> Result<()> {
        match code {
            KeyCode::Char('q') => {
                if !self.tabs.is_empty() {
                    self.tabs.remove(self.active);
                    self.active = self.active.min(self.tabs.len().saturating_sub(1));
                }
                if self.tabs.is_empty() {
                    self.quitting = true;
                }
            }
            KeyCode::Esc => {
                if let Some(tab) = self.tab().filter(|tab| !tab.query.is_empty()) {
                    tab.search(String::new());
                } else {
                    self.quitting = true;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if !self.tabs.is_empty() {
                    let count = self.tabs.len();
                    self.active =
                        (self.active + if code == KeyCode::Tab { 1 } else { count - 1 }) % count;
                }
            }
            KeyCode::Char('f') => {
                let path = self
                    .tabs
                    .get(self.active)
                    .map(|tab| tab.document.source.directory())
                    .unwrap_or(Path::new("."))
                    .to_owned();
                self.browse(&path)?;
            }
            KeyCode::Char('o') => self.mode = Mode::Prompt(Prompt::Open, String::new()),
            KeyCode::Char('/') => self.mode = Mode::Prompt(Prompt::Search, String::new()),
            KeyCode::Char(':') => self.mode = Mode::Prompt(Prompt::Percent, String::new()),
            KeyCode::Char('?') => self.mode = Mode::Help(0),
            KeyCode::Char('t') => {
                let items = self
                    .tabs
                    .get(self.active)
                    .map(|tab| {
                        tab.document
                            .entries
                            .iter()
                            .enumerate()
                            .filter_map(|(index, entry)| {
                                entry.level.map(|level| Item {
                                    label: format!(
                                        "{}{}",
                                        "  ".repeat(level.saturating_sub(1) as usize),
                                        entry.text,
                                    ),
                                    action: Action::Entry(index),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.list("Outline — / filter, Enter jump".into(), items);
            }
            KeyCode::Enter | KeyCode::Char('L') => {
                let items = self
                    .tabs
                    .get(self.active)
                    .map(|tab| {
                        tab.document
                            .entries
                            .iter()
                            .filter(|entry| entry.level.is_none())
                            .filter_map(|entry| {
                                entry.target.as_ref().map(|target| Item {
                                    label: format!("{} → {}", entry.text, target),
                                    action: Action::Link(target.clone()),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.list("Links — / filter, Enter open".into(), items);
            }
            _ => {
                let Some(tab) = self.tab() else {
                    return Ok(());
                };
                match code {
                    KeyCode::Char('g') | KeyCode::Home => tab.jump_percent(0, view),
                    KeyCode::Char('G') | KeyCode::End => tab.jump_percent(100, view),
                    KeyCode::Char('b') => tab.history(false),
                    KeyCode::Char('B') => tab.history(true),
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        if !tab.next_match(code == KeyCode::Char('N')) {
                            self.message = "No matches; / to search".into();
                        }
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => tab.rebuild(tab.options.dark)?,
                    KeyCode::Char('i') => tab.rebuild(!tab.options.dark)?,
                    KeyCode::Char('+')
                    | KeyCode::Char('=')
                    | KeyCode::Char('-')
                    | KeyCode::Char('0') => {
                        let ppi = match code {
                            KeyCode::Char('0') => tab.base_ppi,
                            KeyCode::Char('-') => (tab.options.ppi as f64 / 1.25).round() as u32,
                            _ => (tab.options.ppi as f64 * 1.25).round() as u32,
                        }
                        .clamp(36, 288);
                        tab.zoom(ppi);
                        tab.fit_width = false;
                    }
                    KeyCode::Char('m') => {
                        tab.fit_width = !tab.fit_width;
                        if tab.fit_width {
                            tab.fit(view);
                        } else {
                            tab.zoom(tab.base_ppi);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn prepare(&mut self, out: &mut impl Write, view: Viewport) -> Result<bool> {
        let started = Instant::now();
        let popup = matches!(self.mode, Mode::List(_) | Mode::Help(_));
        if let Some(tab) = self
            .tabs
            .get_mut(self.active)
            .filter(|_| !popup && view.width > 0 && view.height > 0 && view.rows > view.top + 1)
        {
            tab.fit(view);
            let direction =
                self.scroll
                    .target
                    .map_or(0, |target| match target.cmp(&tab.position.y) {
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    });
            self.cache.request(
                &tab.document,
                tab.generation,
                tab.options.ppi,
                tab.position.x,
                tab.position.y,
                view,
                direction,
                self.scroll.next_position(tab.position.y),
            );
            let work = self.cache.service(out)?;
            let performance = self.performance.get_or_insert_with(Performance::default);
            if work.completed {
                performance.render = work.render;
                performance.encode = work.encode;
            }
            performance.upload = work.upload;
            performance.cache_hit = self.cache.ready_at(tab.position.x, tab.position.y);
            performance.blocking = started.elapsed();
            Ok(self.cache.changed())
        } else {
            self.cache.cancel();
            self.cache.prune(out)?;
            self.performance = None;
            Ok(false)
        }
    }

    fn draw(&mut self, out: &mut impl Write, view: Viewport) -> Result<()> {
        // Worker raster/PNG work and prepare() uploads finish outside synchronized
        // output, so a slow strip cannot expire the terminal's update transaction.
        out.write_all(b"\x1b[?2026h")?;
        let started = Instant::now();
        let result = self.draw_inner(&mut FrameOutput(out), view);
        let end = out.write_all(b"\x1b[?2026l").and_then(|()| out.flush());
        result.and(end.map_err(Into::into))?;
        self.cache.prune(out)?;
        if let Some(performance) = &mut self.performance {
            performance.output = started.elapsed();
        }
        // Report the completed image output, including its actual flush, not terminal latency.
        out.write_all(b"\x1b[?2026h")?;
        let result = self.draw_status(out, view);
        let end = out.write_all(b"\x1b[?2026l").and_then(|()| out.flush());
        result.and(end).map_err(Into::into)
    }

    fn draw_inner(&mut self, out: &mut impl Write, view: Viewport) -> Result<()> {
        let popup = matches!(self.mode, Mode::List(_) | Mode::Help(_));
        queue!(
            out,
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(if popup { theme::BG_DARK1 } else { theme::BG }),
            SetForegroundColor(theme::FG)
        )?;
        if popup {
            for row in 0..view.rows {
                queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            }
            graphics::clear(out)?;
            match &self.mode {
                Mode::List(list) => draw_list(out, view, list)?,
                Mode::Help(offset) => draw_help(out, view, *offset)?,
                _ => {}
            }
        } else {
            if let Some(tab) = self.tabs.get(self.active) {
                if view.width > 0 && view.height > 0 && view.rows > view.top + 1 {
                    if self.cache.ready_at(tab.position.x, tab.position.y) {
                        // ED2 also deletes Kitty graphics. Erase text only, then replace
                        // the complete visible strip set in one synchronized update.
                        for row in view.top..view.rows.saturating_sub(1) {
                            queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
                        }
                        self.cache.place(out, view)?;
                    }
                    // Jumps/reloads retain the previous complete view until every new
                    // visible strip is ready; partial viewports are never placed.
                } else {
                    graphics::clear(out)?;
                }
            } else {
                graphics::clear(out)?;
            }
            self.draw_tabs(out, view)?;
        }
        queue!(
            out,
            SetBackgroundColor(theme::BG),
            SetForegroundColor(theme::FG)
        )?;
        Ok(())
    }

    fn draw_tabs(&self, out: &mut impl Write, view: Viewport) -> io::Result<()> {
        if self.tabs.len() < 2 || view.rows < 2 {
            return Ok(());
        }
        queue!(
            out,
            MoveTo(0, 0),
            SetBackgroundColor(theme::BG_DARK1),
            SetForegroundColor(theme::DARK3),
            Clear(ClearType::CurrentLine)
        )?;
        let mut remaining = view.columns.saturating_sub(1);
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let name = tab.document.source.name().to_string_lossy();
            queue!(
                out,
                SetBackgroundColor(if active { theme::BLUE } else { theme::BG }),
                SetForegroundColor(if active { theme::BG_DARK } else { theme::DARK3 }),
                SetAttribute(if active {
                    Attribute::Bold
                } else {
                    Attribute::NormalIntensity
                })
            )?;
            segment(out, &format!(" {}:{} ", index + 1, name), &mut remaining)?;
        }
        queue!(out, SetAttribute(Attribute::NormalIntensity))
    }

    fn draw_status(&self, out: &mut impl Write, view: Viewport) -> io::Result<()> {
        if view.rows == 0 {
            return Ok(());
        }
        queue!(
            out,
            MoveTo(0, view.rows - 1),
            SetBackgroundColor(theme::BG_DARK),
            SetForegroundColor(theme::FG),
            Clear(ClearType::CurrentLine)
        )?;
        let mut remaining = view.columns.saturating_sub(1);
        segment(out, " ", &mut remaining)?;
        match &self.mode {
            Mode::Prompt(prompt, text) => {
                queue!(out, SetForegroundColor(theme::CYAN))?;
                segment(
                    out,
                    match prompt {
                        Prompt::Search => "/",
                        Prompt::Open => "Open: ",
                        Prompt::Percent => "Position (0–100%): ",
                    },
                    &mut remaining,
                )?;
                queue!(out, SetForegroundColor(theme::FG))?;
                segment(out, text, &mut remaining)?;
            }
            Mode::List(_) | Mode::Help(_) => {
                queue!(out, SetForegroundColor(theme::COMMENT))?;
                segment(out, "esc: close", &mut remaining)?;
            }
            Mode::Normal => {
                if let Some(tab) = self.tabs.get(self.active) {
                    queue!(out, SetForegroundColor(theme::BLUE))?;
                    segment(out, &format!("{}%", tab.percent(view)), &mut remaining)?;
                    queue!(out, SetForegroundColor(theme::YELLOW))?;
                    segment(
                        out,
                        &format!(
                            "  {}% {}",
                            zoom_percent(tab.options.ppi),
                            if tab.fit_width { "fit width" } else { "actual" },
                        ),
                        &mut remaining,
                    )?;
                    if !tab.matches.is_empty() {
                        queue!(out, SetForegroundColor(theme::MAGENTA))?;
                        segment(
                            out,
                            &format!("  match {}/{}", tab.match_index + 1, tab.matches.len()),
                            &mut remaining,
                        )?;
                    }
                } else {
                    queue!(out, SetForegroundColor(theme::COMMENT))?;
                    segment(out, "f: browse  o: open  q: quit", &mut remaining)?;
                }
                if let Some(performance) = self.performance {
                    queue!(out, SetForegroundColor(theme::COMMENT))?;
                    let cache = if performance.cache_hit { " cache" } else { "" };
                    if self.show_performance {
                        segment(
                            out,
                            &format!(
                                "  bg render {:.1}ms  bg PNG {:.1}ms  upload {:.1}ms  output {:.1}ms{cache}",
                                performance.render.as_secs_f64() * 1000.0,
                                performance.encode.as_secs_f64() * 1000.0,
                                performance.upload.as_secs_f64() * 1000.0,
                                performance.output.as_secs_f64() * 1000.0,
                            ),
                            &mut remaining,
                        )?;
                    } else {
                        segment(
                            out,
                            &format!(
                                "  frame {:.1}ms{cache}",
                                (performance.blocking + performance.output).as_secs_f64() * 1000.0,
                            ),
                            &mut remaining,
                        )?;
                    }
                }
                if !self.message.is_empty() {
                    queue!(out, SetForegroundColor(theme::GREEN))?;
                    segment(out, &format!("  {}", self.message), &mut remaining)?;
                }
                queue!(out, SetForegroundColor(theme::COMMENT))?;
                segment(out, "  ?: help", &mut remaining)?;
            }
        }
        Ok(())
    }
}

mod theme {
    use crossterm::style::Color;

    pub const BG: Color = Color::Rgb {
        r: 0x22,
        g: 0x24,
        b: 0x36,
    };
    pub const BG_DARK: Color = Color::Rgb {
        r: 0x1e,
        g: 0x20,
        b: 0x30,
    };
    pub const BG_DARK1: Color = Color::Rgb {
        r: 0x19,
        g: 0x1b,
        b: 0x29,
    };
    pub const BG_HIGHLIGHT: Color = Color::Rgb {
        r: 0x2f,
        g: 0x33,
        b: 0x4d,
    };
    pub const BLUE: Color = Color::Rgb {
        r: 0x82,
        g: 0xaa,
        b: 0xff,
    };
    pub const BLUE1: Color = Color::Rgb {
        r: 0x65,
        g: 0xbc,
        b: 0xff,
    };
    pub const BLUE7: Color = Color::Rgb {
        r: 0x39,
        g: 0x4b,
        b: 0x70,
    };
    pub const COMMENT: Color = Color::Rgb {
        r: 0x63,
        g: 0x6d,
        b: 0xa6,
    };
    pub const FG: Color = Color::Rgb {
        r: 0xc8,
        g: 0xd3,
        b: 0xf5,
    };
    pub const FG_DARK: Color = Color::Rgb {
        r: 0x82,
        g: 0x8b,
        b: 0xb8,
    };
    pub const DARK3: Color = Color::Rgb {
        r: 0x54,
        g: 0x5c,
        b: 0x7e,
    };
    pub const GREEN: Color = Color::Rgb {
        r: 0xc3,
        g: 0xe8,
        b: 0x8d,
    };
    pub const MAGENTA: Color = Color::Rgb {
        r: 0xc0,
        g: 0x99,
        b: 0xff,
    };
    pub const YELLOW: Color = Color::Rgb {
        r: 0xff,
        g: 0xc7,
        b: 0x77,
    };
    pub const CYAN: Color = Color::Rgb {
        r: 0x86,
        g: 0xe1,
        b: 0xfc,
    };
}

const HELP_NAVIGATION: &[(&str, &str)] = &[
    ("j/k · ↑/↓ · wheel", "Smooth vertical scroll (S toggles)"),
    ("h/l · ←/→", "Pan horizontally"),
    ("Space · PgDn", "Scroll forward 85% viewport"),
    ("Backspace · PgUp", "Scroll backward 85% viewport"),
    ("g/G · Home/End", "Document top / bottom"),
    (":", "Jump to percentage"),
    ("/", "Search text blocks"),
    ("n / N", "Next / previous match"),
    ("t", "Heading outline"),
    ("Enter / L", "Browse links"),
    ("b / B", "Back / forward"),
    ("Tab / Shift-Tab", "Switch tabs"),
    ("Alt-1 … Alt-9", "Select tab"),
];
const HELP_VIEWER: &[(&str, &str)] = &[
    ("m", "Fit width / actual"),
    ("+ / - / 0", "Zoom in / out / reset"),
    ("S", "Toggle smooth scrolling (on by default)"),
    ("p", "Toggle detailed render / encode / output timings"),
    ("i", "Toggle light / dark"),
    ("r / R", "Reload document"),
    ("f", "Browse files in a new tab"),
    ("o", "Open path in a new tab"),
    ("q", "Close pane / active tab"),
    ("Esc", "Close / clear search / exit"),
    ("Ctrl-c", "Exit immediately"),
    ("?", "Keyboard shortcuts"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Rect {
    fn inner(self) -> Self {
        Self {
            x: self.x.saturating_add(1),
            y: self.y.saturating_add(1),
            width: self.width.saturating_sub(2),
            height: self.height.saturating_sub(2),
        }
    }
}

fn picker_rect(columns: u16, rows: u16) -> Rect {
    let width = if columns > 4 {
        (u32::from(columns) * 3 / 4)
            .max(50)
            .min(u32::from(columns - 4)) as u16
    } else {
        columns
    };
    let height = if rows > 4 {
        (u32::from(rows) * 3 / 4).max(6).min(u32::from(rows - 2)) as u16
    } else {
        rows
    };
    Rect {
        x: columns.saturating_sub(width) / 2,
        y: rows.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn segment(out: &mut impl Write, text: &str, remaining: &mut u16) -> io::Result<()> {
    let text = terminal_text(text, usize::from(*remaining));
    let width: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    *remaining = remaining.saturating_sub(width as u16);
    queue!(out, Print(text))
}

fn surface_row(out: &mut impl Write, rect: Rect, row: u16, bg: Color) -> io::Result<()> {
    if row >= rect.height || rect.width == 0 {
        return Ok(());
    }
    queue!(
        out,
        MoveTo(rect.x, rect.y + row),
        SetBackgroundColor(bg),
        Print(" ".repeat(usize::from(rect.width))),
        MoveTo(rect.x, rect.y + row)
    )
}

fn pane(out: &mut impl Write, rect: Rect, title: &str) -> io::Result<Rect> {
    for row in 0..rect.height {
        surface_row(out, rect, row, theme::BG)?;
    }
    if rect.width < 2 || rect.height < 2 {
        return Ok(Rect {
            width: 0,
            height: 0,
            ..rect
        });
    }
    queue!(out, SetForegroundColor(theme::BLUE7))?;
    let horizontal = "─".repeat(usize::from(rect.width - 2));
    queue!(
        out,
        MoveTo(rect.x, rect.y),
        Print(format!("┌{horizontal}┐"))
    )?;
    for row in 1..rect.height - 1 {
        queue!(
            out,
            MoveTo(rect.x, rect.y + row),
            Print("│"),
            MoveTo(rect.x + rect.width - 1, rect.y + row),
            Print("│")
        )?;
    }
    queue!(
        out,
        MoveTo(rect.x, rect.y + rect.height - 1),
        Print(format!("└{horizontal}┘")),
        MoveTo(rect.x + 1, rect.y),
        SetForegroundColor(theme::BLUE),
        SetAttribute(Attribute::Bold)
    )?;
    let mut remaining = rect.width - 2;
    segment(out, &format!(" {title} "), &mut remaining)?;
    queue!(out, SetAttribute(Attribute::NormalIntensity))?;
    Ok(rect.inner())
}

fn draw_list(out: &mut impl Write, view: Viewport, list: &List) -> io::Result<()> {
    let rect = picker_rect(view.columns.saturating_sub(1), view.rows.saturating_sub(1));
    let inner = pane(out, rect, &list.title)?;
    if inner.width == 0 || inner.height == 0 {
        return Ok(());
    }
    surface_row(out, inner, 0, theme::BG_DARK)?;
    let mut remaining = inner.width;
    queue!(out, SetForegroundColor(theme::BLUE))?;
    segment(out, " / ", &mut remaining)?;
    queue!(
        out,
        SetForegroundColor(if list.filter.is_empty() && !list.editing {
            theme::COMMENT
        } else {
            theme::FG
        })
    )?;
    segment(
        out,
        if list.filter.is_empty() && !list.editing {
            "to filter"
        } else {
            &list.filter
        },
        &mut remaining,
    )?;
    let visible = list.visible();
    let count = usize::from(inner.height.saturating_sub(2));
    let start = list.selected.saturating_sub(count.saturating_sub(1));
    for (row, &index) in visible.iter().skip(start).take(count).enumerate() {
        let selected = start + row == list.selected;
        let item = &list.items[index];
        surface_row(
            out,
            inner,
            row as u16 + 1,
            if selected {
                theme::BG_HIGHLIGHT
            } else {
                theme::BG
            },
        )?;
        let mut remaining = inner.width;
        queue!(out, SetForegroundColor(theme::BLUE))?;
        segment(out, if selected { "▌ " } else { "  " }, &mut remaining)?;
        queue!(
            out,
            SetForegroundColor(match &item.action {
                Action::Directory(_) => theme::BLUE1,
                Action::Link(_) => theme::CYAN,
                _ => theme::FG,
            }),
            SetAttribute(if selected {
                Attribute::Bold
            } else {
                Attribute::NormalIntensity
            })
        )?;
        segment(out, &item.label, &mut remaining)?;
        queue!(out, SetAttribute(Attribute::NormalIntensity))?;
    }
    if visible.is_empty() && count > 0 {
        surface_row(out, inner, 1, theme::BG)?;
        queue!(out, SetForegroundColor(theme::FG_DARK))?;
        let mut remaining = inner.width;
        segment(out, "   No matches", &mut remaining)?;
    }
    if inner.height > 1 {
        surface_row(out, inner, inner.height - 1, theme::BG_DARK)?;
        let mut remaining = inner.width;
        queue!(out, SetForegroundColor(theme::BLUE))?;
        segment(out, " j/k", &mut remaining)?;
        queue!(out, SetForegroundColor(theme::COMMENT))?;
        segment(out, " select  ", &mut remaining)?;
        queue!(out, SetForegroundColor(theme::BLUE))?;
        segment(out, "enter", &mut remaining)?;
        queue!(out, SetForegroundColor(theme::COMMENT))?;
        segment(out, " open  / filter  esc close", &mut remaining)?;
    }
    Ok(())
}

fn help_lines(
    out: &mut impl Write,
    rect: Rect,
    lines: impl Iterator<Item = (&'static str, Option<&'static str>)>,
    count: usize,
    offset: usize,
) -> io::Result<()> {
    let start = offset.min(count.saturating_sub(usize::from(rect.height)));
    for (row, (key, action)) in lines.skip(start).take(usize::from(rect.height)).enumerate() {
        surface_row(out, rect, row as u16, theme::BG)?;
        let mut remaining = rect.width;
        if let Some(action) = action {
            let key_width = rect.width.saturating_sub(2).min(24).min(rect.width / 2);
            queue!(
                out,
                SetForegroundColor(theme::BLUE),
                SetBackgroundColor(theme::BG_HIGHLIGHT),
                SetAttribute(Attribute::Bold)
            )?;
            let key = terminal_text(key, usize::from(key_width));
            let used: usize = key.chars().map(|c| c.width().unwrap_or(0)).sum();
            segment(out, &key, &mut remaining)?;
            segment(
                out,
                &" ".repeat(usize::from(key_width).saturating_sub(used)),
                &mut remaining,
            )?;
            queue!(
                out,
                SetAttribute(Attribute::NormalIntensity),
                SetBackgroundColor(theme::BG),
                SetForegroundColor(theme::FG)
            )?;
            segment(out, &format!(" {action}"), &mut remaining)?;
        } else {
            queue!(
                out,
                SetForegroundColor(theme::BLUE1),
                SetAttribute(Attribute::Bold)
            )?;
            segment(out, key, &mut remaining)?;
            queue!(out, SetAttribute(Attribute::NormalIntensity))?;
        }
    }
    Ok(())
}

fn draw_help(out: &mut impl Write, view: Viewport, offset: usize) -> io::Result<()> {
    let rect = picker_rect(view.columns.saturating_sub(1), view.rows.saturating_sub(1));
    let inner = pane(out, rect, "Help")?;
    if inner.width == 0 || inner.height == 0 {
        return Ok(());
    }
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let navigation = std::iter::once(("Navigation", None)).chain(
        HELP_NAVIGATION
            .iter()
            .map(|&(key, action)| (key, Some(action))),
    );
    let viewer = std::iter::once(("Viewer", None))
        .chain(HELP_VIEWER.iter().map(|&(key, action)| (key, Some(action))));
    if body.width >= 90 {
        let width = body.width.saturating_sub(2) / 2;
        help_lines(
            out,
            Rect { width, ..body },
            navigation,
            HELP_NAVIGATION.len() + 1,
            offset,
        )?;
        help_lines(
            out,
            Rect {
                x: body.x + width + 2,
                width: body.width - width - 2,
                ..body
            },
            viewer,
            HELP_VIEWER.len() + 1,
            offset,
        )?;
    } else {
        help_lines(
            out,
            body,
            navigation.chain(std::iter::once(("", None))).chain(viewer),
            HELP_NAVIGATION.len() + HELP_VIEWER.len() + 3,
            offset,
        )?;
    }
    surface_row(out, inner, inner.height - 1, theme::BG_DARK)?;
    queue!(out, SetForegroundColor(theme::COMMENT))?;
    let mut remaining = inner.width;
    segment(out, " j/k scroll  ? / esc / q close", &mut remaining)?;
    Ok(())
}

fn terminal_text(text: &str, columns: usize) -> String {
    let mut result = String::new();
    let mut width = 0;
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        let w = c.width().unwrap_or(0);
        if width + w > columns {
            break;
        }
        result.push(c);
        width += w;
    }
    result
}

fn markdown(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        ["md", "markdown", "mdown", "mkd", "mdx"]
            .iter()
            .any(|expected| ext.eq_ignore_ascii_case(expected))
    })
}

fn decode(text: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut iter = text.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let a = iter.next().and_then(|b| (b as char).to_digit(16));
            let b = iter.next().and_then(|b| (b as char).to_digit(16));
            match (a, b) {
                (Some(a), Some(b)) => bytes.push((a * 16 + b) as u8),
                _ => bail!("invalid percent-encoded link"),
            }
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).context("link is not valid UTF-8")
}

pub fn run(sources: Vec<Arc<Source>>, options: RenderOptions) -> Result<()> {
    let (sender, events) = mpsc::channel();
    let raster_sender = sender.clone();
    let mut app = App {
        tabs: vec![],
        active: 0,
        mode: Mode::Normal,
        message: String::new(),
        defaults: options,
        quitting: false,
        scroll: SmoothScroll::new(Instant::now()),
        performance: None,
        show_performance: false,
        cache: StripCache::new(move || {
            let _ = raster_sender.send(Wake::Raster);
        })?,
    };
    for source in sources {
        app.tabs.push(Tab::open(
            source,
            RenderOptions {
                dark: app.defaults.dark,
                ppi: app.defaults.ppi,
            },
        )?);
    }
    if app.tabs.is_empty() {
        app.browse(Path::new("."))?;
    }
    let mut view = graphics::viewport(u16::from(app.tabs.len() > 1))?;
    let _guard = TerminalGuard::enter()?;
    let _input = InputReader::start(sender)?;
    let mut out = io::stdout();
    let mut dirty = true;
    let mut last_check = Instant::now();
    while !app.quitting {
        let top = u16::from(app.tabs.len() > 1);
        if view.top != top {
            view = graphics::viewport(top)?;
        }
        dirty |= app.prepare(&mut out, view)?;
        if dirty {
            app.draw(&mut out, view)?;
            dirty = false;
        }
        let wait = if app.cache.pending_upload() {
            // Each immediate pass uploads one ready strip; never spin awaiting a worker.
            Duration::ZERO
        } else {
            app.scroll.wait(Instant::now())
        };
        let wake = match events.recv_timeout(wait) {
            Ok(wake) => Some(wake),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("terminal input stopped"),
        };
        if let Some(Wake::Input(input)) = wake {
            let result = match input? {
                Event::Key(key) => app.key(key, view),
                Event::Resize(_, _) => {
                    app.scroll.cancel();
                    view = graphics::viewport(u16::from(app.tabs.len() > 1))?;
                    Ok(true)
                }
                Event::Mouse(mouse) if matches!(app.mode, Mode::Normal) => {
                    let delta = match mouse.kind {
                        MouseEventKind::ScrollDown => 48,
                        MouseEventKind::ScrollUp => -48,
                        _ => 0,
                    };
                    Ok(delta != 0 && app.scroll_vertical(delta, view))
                }
                _ => Ok(false),
            };
            match result {
                Ok(changed) => dirty |= changed,
                Err(error) => {
                    app.message = format!("Error: {error:#}");
                    dirty = true;
                }
            }
        }
        dirty |= app.advance_scroll(Instant::now());
        if last_check.elapsed() >= Duration::from_millis(750) {
            last_check = Instant::now();
            if let Some(tab) = app.tabs.get_mut(app.active) {
                let current = stamp(&tab.document.source);
                if current != tab.stamp {
                    app.scroll.cancel();
                    // A failed edit leaves the old document usable; retry on the next edit or r.
                    tab.stamp = current;
                    if let Err(error) = tab.rebuild(tab.options.dark) {
                        app.message = format!("Reload failed: {error:#}");
                    } else {
                        app.message.clear();
                        // Outline actions contain indices into the previous document.
                        if matches!(&app.mode, Mode::List(list) if list.items.iter().any(
                            |item| matches!(&item.action, Action::Entry(_))
                        )) {
                            app.mode = Mode::Normal;
                        }
                    }
                    dirty = true;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooth_scroll_accumulates_and_advances_only_when_due() {
        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut position = 100;
        assert!(!scroll.scroll(&mut position, 100, 1000, now));
        assert!(!scroll.scroll(&mut position, 100, 1000, now + Duration::from_millis(8)));
        assert_eq!(scroll.target, Some(300));
        assert_eq!(
            scroll.wait(now + Duration::from_millis(8)),
            Duration::from_millis(8)
        );
        assert!(!scroll.advance(&mut position, now + Duration::from_millis(15)));
        assert_eq!(position, 100);
        assert!(scroll.advance(&mut position, now + SCROLL_FRAME));
        assert_eq!(position, 150);
        assert!(!scroll.advance(&mut position, now + SCROLL_FRAME));
        assert!(scroll.advance(&mut position, now + SCROLL_FRAME * 2));
        assert_eq!(position, 188);
        // A delayed frame performs one proportional step rather than a burst of catch-up ticks.
        assert!(scroll.advance(&mut position, now + Duration::from_secs(1)));
        assert_eq!(position, 216);
    }

    #[test]
    fn smooth_scroll_finishes_exactly_and_reverses_from_displayed_position() {
        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut position = 100;
        scroll.scroll(&mut position, 100, 1000, now);
        scroll.advance(&mut position, now + SCROLL_FRAME);
        assert_eq!(position, 125);
        scroll.scroll(&mut position, -10, 1000, now + SCROLL_FRAME);
        assert_eq!(scroll.target, Some(115));
        let mut tick = now + SCROLL_FRAME;
        for _ in 0..20 {
            tick += SCROLL_FRAME;
            scroll.advance(&mut position, tick);
        }
        assert_eq!(position, 115);
        assert_eq!(scroll.target, None);
    }

    #[test]
    fn smooth_scroll_clamps_targets_without_invisible_backlog() {
        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut position = 95;
        scroll.scroll(&mut position, i64::MAX, 100, now);
        scroll.scroll(&mut position, i64::MAX, 100, now);
        assert_eq!(scroll.target, Some(100));
        scroll.scroll(&mut position, -10, 100, now);
        assert_eq!(scroll.target, Some(85));
        scroll.scroll(&mut position, i64::MIN, 100, now);
        assert_eq!(scroll.target, Some(0));
        scroll.cancel();
        position = 0;
        scroll.scroll(&mut position, -100, 0, now);
        assert_eq!(scroll.target, None);
        assert!(!scroll.advance(&mut position, now + SCROLL_FRAME));
    }

    #[test]
    fn smooth_scroll_toggle_flushes_and_disabled_scroll_is_immediate() {
        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut position = 50;
        assert!(scroll.enabled);
        scroll.scroll(&mut position, 100, 1000, now);
        scroll.toggle(&mut position);
        assert_eq!(position, 150);
        assert_eq!(scroll.target, None);
        assert!(!scroll.enabled);
        assert!(scroll.scroll(&mut position, 25, 1000, now));
        assert_eq!(position, 175);
        scroll.toggle(&mut position);
        scroll.scroll(&mut position, 100, 1000, now);
        assert_eq!(position, 175);
        assert_eq!(scroll.target, Some(275));
    }

    #[test]
    fn canceled_scroll_cannot_move_a_jump_or_next_document() {
        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut position = 100;
        scroll.scroll(&mut position, 500, 1000, now);
        scroll.cancel();
        position = 900;
        assert!(!scroll.advance(&mut position, now + SCROLL_FRAME));
        assert_eq!(position, 900);
        // A newly scheduled animation has a fresh deadline, not the canceled one's tick.
        scroll.scroll(&mut position, -100, 1000, now + SCROLL_FRAME * 10);
        assert!(!scroll.advance(&mut position, now + SCROLL_FRAME * 10));
        assert!(scroll.advance(&mut position, now + SCROLL_FRAME * 11));
        assert_eq!(position, 875);
    }

    #[test]
    fn displayed_zoom_uses_fixed_96_ppi_reference() {
        assert_eq!(zoom_percent(96), 100);
        assert_eq!(zoom_percent(240), 250);
        assert_eq!(zoom_percent(288), 300);
    }

    #[test]
    fn terminal_strings_cannot_inject_controls_or_overflow_wide_cells() {
        assert_eq!(terminal_text("a\x1b[2J\n界z", 9), "a [2J 界z");
        assert_eq!(terminal_text("a界b", 2), "a");
        assert_eq!(terminal_text("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn links_decode_utf8_without_treating_plus_as_space() {
        assert_eq!(decode("caf%C3%A9+a.md").unwrap(), "café+a.md");
        assert!(decode("file%2").is_err());
        assert!(decode("%FF").is_err());
    }

    #[test]
    fn popup_geometry_stays_inside_even_the_smallest_terminal() {
        for columns in 0..100 {
            for rows in 0..40 {
                let rect = picker_rect(columns, rows);
                assert!(rect.x + rect.width <= columns);
                assert!(rect.y + rect.height <= rows);
            }
        }
        assert_eq!(
            picker_rect(100, 40),
            Rect {
                x: 12,
                y: 5,
                width: 75,
                height: 30
            }
        );
    }

    #[test]
    fn styled_segments_share_a_unicode_cell_limit() {
        let mut out = Vec::new();
        let mut remaining = 3;
        segment(&mut out, "界", &mut remaining).unwrap();
        segment(&mut out, "a界", &mut remaining).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "界a");
        assert_eq!(remaining, 0);
    }
}
