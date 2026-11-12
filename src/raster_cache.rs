use std::{
    collections::VecDeque,
    io::Write,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    graphics::{self, Frame, Viewport},
    render::Document,
};

const STRIP_PIXELS: u32 = 1024 * 1024;
const STRIP_HEIGHT: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextKey {
    generation: u64,
    ppi: u32,
    x: u32,
    width: u32,
    height: u32,
    viewport_width: u32,
    viewport_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StripKey {
    context: ContextKey,
    y: u32,
    height: u32,
}

impl ContextKey {
    fn append(&self, keys: &mut Vec<StripKey>, start: u32, end: u32) {
        if start >= end || self.width == 0 {
            return;
        }
        let stride = STRIP_HEIGHT.min(STRIP_PIXELS / self.width).max(1);
        let mut y = start / stride * stride;
        while y < end {
            let key = StripKey {
                context: *self,
                y,
                height: stride.min(self.height - y),
            };
            if !keys.contains(&key) {
                keys.push(key);
            }
            y = y.saturating_add(stride);
        }
    }

    fn plan(
        &self,
        y: u32,
        direction: i32,
        imminent: Option<u32>,
        keys: &mut Vec<StripKey>,
    ) -> usize {
        keys.clear();
        let end = y.saturating_add(self.viewport_height).min(self.height);
        self.append(keys, y, end);
        let visible = keys.len();
        let ahead = self.viewport_height.saturating_mul(2);
        if let Some(next) = imminent {
            let next_end = next.saturating_add(self.viewport_height).min(self.height);
            self.append(keys, next, next_end);
            if next >= end || next_end <= y {
                // A burst can put the next eased step outside ordinary overscan.
                // The displayed viewport supplies the behind buffer during this jump.
                if direction < 0 {
                    self.append(keys, next.saturating_sub(ahead), next);
                } else {
                    self.append(
                        keys,
                        next_end,
                        next_end.saturating_add(ahead).min(self.height),
                    );
                }
                return visible;
            }
        }
        if direction < 0 {
            self.append(keys, y.saturating_sub(ahead), y);
            self.append(
                keys,
                end,
                end.saturating_add(self.viewport_height).min(self.height),
            );
        } else {
            self.append(keys, end, end.saturating_add(ahead).min(self.height));
            self.append(keys, y.saturating_sub(self.viewport_height), y);
        }
        visible
    }
}

#[derive(Default)]
struct Queue {
    epoch: u64,
    wanted: Vec<StripKey>,
    pending: VecDeque<StripKey>,
    active: Option<(u64, StripKey)>,
}

impl Queue {
    fn replace(&mut self, epoch: u64, wanted: &[StripKey]) {
        self.epoch = epoch;
        self.wanted.clear();
        self.wanted.extend_from_slice(wanted);
        self.pending.clear();
        let active = self.active;
        self.pending.extend(
            wanted
                .iter()
                .copied()
                .filter(|key| active != Some((epoch, *key))),
        );
    }

    fn accepts(&self, epoch: u64, key: StripKey) -> bool {
        self.epoch == epoch && self.wanted.contains(&key)
    }

    fn next(&mut self) -> Option<(u64, StripKey)> {
        let key = self.pending.pop_front()?;
        let job = (self.epoch, key);
        self.active = Some(job);
        Some(job)
    }
}

struct Completed {
    epoch: u64,
    key: StripKey,
    result: Result<(Frame, Duration, Duration)>,
}

#[derive(Default)]
struct WorkerState {
    queue: Queue,
    document: Option<Arc<Document>>,
    completed: VecDeque<Completed>,
    notified: bool,
    stop: bool,
}

struct Worker {
    shared: Arc<(Mutex<WorkerState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn new(wake: impl Fn() + Send + 'static) -> Result<Self> {
        let shared = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let state = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("mdterm-raster".into())
            .spawn(move || {
                let (mutex, ready) = &*state;
                loop {
                    let (epoch, key, document) = {
                        let mut state = mutex.lock().unwrap();
                        while !state.stop && state.queue.pending.is_empty() {
                            state = ready.wait(state).unwrap();
                        }
                        if state.stop {
                            break;
                        }
                        let (epoch, key) = state.queue.next().unwrap();
                        (epoch, key, Arc::clone(state.document.as_ref().unwrap()))
                    };
                    let raster = document.rasterize(
                        key.context.ppi,
                        key.context.x,
                        key.y,
                        key.context.width,
                        key.height,
                    );
                    // Rendering is not interruptible, but obsolete work never starts encoding.
                    {
                        let mut state = mutex.lock().unwrap();
                        if state.stop || !state.queue.accepts(epoch, key) {
                            state.queue.active = None;
                            continue;
                        }
                    }
                    let result = raster.and_then(|raster| {
                        let started = Instant::now();
                        let frame = Frame::from_pixels(raster.pixels)?;
                        Ok((frame, raster.render_elapsed, started.elapsed()))
                    });
                    let notify = {
                        let mut state = mutex.lock().unwrap();
                        state.queue.active = None;
                        if state.stop || !state.queue.accepts(epoch, key) {
                            continue;
                        }
                        state.completed.push_back(Completed { epoch, key, result });
                        let notify = !state.notified;
                        state.notified = true;
                        notify
                    };
                    if notify {
                        wake();
                    }
                }
            })
            .context("cannot start raster worker")?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn request(&self, epoch: u64, document: &Arc<Document>, missing: &[StripKey]) {
        let (mutex, ready) = &*self.shared;
        let mut state = mutex.lock().unwrap();
        if state.queue.epoch == epoch && state.queue.wanted == missing {
            return;
        }
        state
            .completed
            .retain(|item| item.epoch == epoch && missing.contains(&item.key));
        // Completed jobs are mailbox entries, not candidates for another render.
        let mut queue = std::mem::take(&mut state.queue);
        queue.replace(epoch, missing);
        queue
            .pending
            .retain(|key| !state.completed.iter().any(|item| item.key == *key));
        state.queue = queue;
        if state
            .document
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, document))
        {
            state.document = Some(Arc::clone(document));
        }
        ready.notify_one();
    }

    fn drain(&self, destination: &mut VecDeque<Completed>) {
        let mut state = self.shared.0.lock().unwrap();
        std::mem::swap(destination, &mut state.completed);
        state.notified = false;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let mut state = self.shared.0.lock().unwrap();
            state.stop = true;
            state.queue.pending.clear();
        }
        self.shared.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Resident {
    key: StripKey,
    frame: Frame,
    uploaded: bool,
}

#[derive(Default)]
pub(super) struct Work {
    pub render: Duration,
    pub encode: Duration,
    pub upload: Duration,
    pub completed: bool,
}

pub(super) struct StripCache {
    worker: Worker,
    context: Option<ContextKey>,
    epoch: u64,
    y: u32,
    direction: i32,
    desired: Vec<StripKey>,
    visible: usize,
    missing: Vec<StripKey>,
    residents: Vec<Resident>,
    displayed: Vec<StripKey>,
    placement_ids: Vec<u32>,
    displayed_position: Option<(ContextKey, u32)>,
    completed: VecDeque<Completed>,
}

impl StripCache {
    pub fn new(wake: impl Fn() + Send + 'static) -> Result<Self> {
        Ok(Self {
            worker: Worker::new(wake)?,
            context: None,
            epoch: 0,
            y: 0,
            direction: 1,
            desired: Vec::new(),
            visible: 0,
            missing: Vec::new(),
            residents: Vec::new(),
            displayed: Vec::new(),
            placement_ids: Vec::new(),
            displayed_position: None,
            completed: VecDeque::new(),
        })
    }

    pub fn request(
        &mut self,
        document: &Arc<Document>,
        generation: u64,
        ppi: u32,
        x: u32,
        y: u32,
        view: Viewport,
        direction: i32,
        imminent: Option<u32>,
    ) {
        let (width, height) = document.dimensions(ppi);
        let context = ContextKey {
            generation,
            ppi,
            x,
            width: view.width.min(width.saturating_sub(x)),
            height,
            viewport_width: view.width,
            viewport_height: view.height,
        };
        if self.context != Some(context) {
            self.epoch = self.epoch.wrapping_add(1);
            self.context = Some(context);
        } else if y != self.y {
            self.direction = if y < self.y { -1 } else { 1 };
        }
        if direction != 0 {
            self.direction = direction;
        }
        self.y = y;
        self.visible = context.plan(y, self.direction, imminent, &mut self.desired);
        self.missing.clear();
        self.missing.extend(
            self.desired
                .iter()
                .copied()
                .filter(|key| !self.residents.iter().any(|resident| resident.key == *key)),
        );
        self.worker.request(self.epoch, document, &self.missing);
    }

    pub fn cancel(&mut self) {
        if self.context.is_none() {
            return;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.context = None;
        self.desired.clear();
        self.visible = 0;
        let mut state = self.worker.shared.0.lock().unwrap();
        state.queue.replace(self.epoch, &[]);
        state.completed.clear();
        state.document = None;
    }

    pub fn service(&mut self, out: &mut impl Write) -> Result<Work> {
        self.worker.drain(&mut self.completed);
        let mut work = Work::default();
        while let Some(completed) = self.completed.pop_front() {
            if completed.epoch != self.epoch || !self.desired.contains(&completed.key) {
                continue;
            }
            let (frame, render, encode) = completed.result?;
            work.render += render;
            work.encode += encode;
            work.completed = true;
            self.residents.push(Resident {
                key: completed.key,
                frame,
                uploaded: false,
            });
        }
        // One bounded upload per pass leaves input and animation deadlines runnable
        // even when an entire prefetch window finishes at once.
        let next = self.desired.iter().find_map(|key| {
            self.residents
                .iter()
                .position(|resident| resident.key == *key && !resident.uploaded)
        });
        if let Some(index) = next {
            let started = Instant::now();
            self.residents[index].frame.upload(out)?;
            out.flush()?;
            self.residents[index].uploaded = true;
            work.upload = started.elapsed();
        }
        self.prune(out)?;
        Ok(work)
    }

    pub fn pending_upload(&self) -> bool {
        self.residents
            .iter()
            .any(|resident| !resident.uploaded && self.desired.contains(&resident.key))
    }

    pub fn ready_at(&self, x: u32, y: u32) -> bool {
        let Some(context) = self.context else {
            return false;
        };
        if x != context.x || context.width == 0 {
            return false;
        }
        let end = y
            .saturating_add(context.viewport_height)
            .min(context.height);
        let stride = STRIP_HEIGHT.min(STRIP_PIXELS / context.width).max(1);
        let mut strip_y = y / stride * stride;
        while strip_y < end {
            if !self.residents.iter().any(|resident| {
                resident.uploaded && resident.key.context == context && resident.key.y == strip_y
            }) {
                return false;
            }
            strip_y = strip_y.saturating_add(stride);
        }
        end > y
    }

    pub fn changed(&self) -> bool {
        self.context.is_some_and(|context| {
            self.ready_at(context.x, self.y) && self.displayed_position != Some((context, self.y))
        })
    }

    pub fn place(&mut self, out: &mut impl Write, view: Viewport) -> Result<bool> {
        let Some(context) = self.context else {
            return Ok(false);
        };
        if !self.ready_at(context.x, self.y) {
            return Ok(false);
        }
        self.placement_ids.clear();
        let left = view.width.saturating_sub(context.width) / 2;
        let top = view.height.saturating_sub(context.height) / 2;
        for key in &self.desired[..self.visible] {
            let resident = self
                .residents
                .iter()
                .find(|resident| resident.key == *key)
                .unwrap();
            let start = self.y.max(key.y);
            let end = self.y.saturating_add(view.height).min(key.y + key.height);
            resident.frame.place(
                out,
                view,
                start - key.y,
                end - start,
                top + start - self.y,
                left,
            )?;
            self.placement_ids.push(resident.frame.id());
        }
        // Preserve every old placement until the entire replacement has been placed.
        graphics::finish_frame(out, &self.placement_ids)?;
        self.displayed.clear();
        self.displayed
            .extend_from_slice(&self.desired[..self.visible]);
        self.displayed_position = Some((context, self.y));
        Ok(true)
    }

    pub fn prune(&mut self, out: &mut impl Write) -> Result<()> {
        let mut index = 0;
        let mut released = false;
        while index < self.residents.len() {
            let key = self.residents[index].key;
            if self.desired.contains(&key) || self.displayed.contains(&key) {
                index += 1;
            } else {
                self.residents[index].frame.release(out)?;
                self.residents.swap_remove(index);
                released = true;
            }
        }
        if released {
            out.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(width: u32, height: u32, viewport_height: u32) -> ContextKey {
        ContextKey {
            generation: 1,
            ppi: 240,
            x: 0,
            width,
            height,
            viewport_width: width,
            viewport_height,
        }
    }

    fn resident(key: StripKey) -> Resident {
        let pixels = resvg::tiny_skia::Pixmap::new(key.context.width, key.height).unwrap();
        Resident {
            key,
            frame: Frame::from_pixels(pixels).unwrap(),
            uploaded: false,
        }
    }

    #[test]
    fn stale_completions_including_errors_cannot_enter_the_current_generation() {
        let mut cache = StripCache::new(|| {}).unwrap();
        let context = context(4, 1000, 500);
        cache.context = Some(context);
        cache.epoch = 2;
        cache.visible = context.plan(0, 1, None, &mut cache.desired);
        let key = cache.desired[0];
        let mut obsolete = key;
        obsolete.context.generation += 1;
        {
            let mut state = cache.worker.shared.0.lock().unwrap();
            for (epoch, key) in [(1, key), (2, obsolete)] {
                state.completed.push_back(Completed {
                    epoch,
                    key,
                    result: Err(anyhow::anyhow!("obsolete failure")),
                });
            }
        }
        cache.service(&mut Vec::new()).unwrap();
        assert!(cache.residents.is_empty());
        assert!(!cache.ready_at(0, 0));
    }

    #[test]
    fn burst_scroll_prepares_its_next_step_without_partial_or_unuploaded_placement() {
        use super::super::{SCROLL_FRAME, SmoothScroll};

        let now = Instant::now();
        let mut scroll = SmoothScroll::new(now);
        let mut y = 0;
        scroll.scroll(&mut y, 90_000, 99_500, now);
        let next = scroll.next_position(y).unwrap();
        let mut context = context(8, 100_000, 500);
        context.viewport_width = 800;
        assert!(next > context.viewport_height * 3);
        let view = Viewport {
            columns: 80,
            rows: 26,
            top: 0,
            width: 800,
            height: 500,
        };
        let mut cache = StripCache::new(|| {}).unwrap();
        cache.context = Some(context);
        cache.visible = context.plan(y, 1, Some(next), &mut cache.desired);
        cache
            .residents
            .extend(cache.desired.iter().copied().map(resident));
        let pixels: u64 = cache
            .desired
            .iter()
            .map(|key| u64::from(key.context.width) * u64::from(key.height))
            .sum();
        assert!(
            pixels
                <= u64::from(context.width)
                    * u64::from(context.viewport_height * 4 + STRIP_HEIGHT * 4)
        );
        let mut out = Vec::new();
        assert!(!cache.place(&mut out, view).unwrap());
        assert!(out.is_empty());
        while !cache.ready_at(0, y) {
            cache.service(&mut out).unwrap();
        }
        assert!(cache.place(&mut out, view).unwrap());
        let displayed = cache.displayed.clone();
        assert!(!cache.ready_at(0, next));
        while cache.pending_upload() {
            let uploaded = cache
                .residents
                .iter()
                .filter(|resident| resident.uploaded)
                .count();
            cache.service(&mut out).unwrap();
            assert_eq!(
                cache
                    .residents
                    .iter()
                    .filter(|resident| resident.uploaded)
                    .count(),
                uploaded + 1,
            );
            assert_eq!(cache.displayed, displayed);
        }
        assert!(cache.ready_at(0, next));
        assert!(scroll.advance(&mut y, now + SCROLL_FRAME));
        assert_eq!(y, next);
        cache.y = y;
        cache.visible = context.plan(y, 1, None, &mut cache.desired);
        assert!(cache.place(&mut out, view).unwrap());
        cache.prune(&mut out).unwrap();
        assert!(
            cache
                .residents
                .iter()
                .all(|resident| cache.desired.contains(&resident.key))
        );
        // Explicitly clean the test's thread-local transport registry.
        cache.desired.clear();
        cache.displayed.clear();
        cache.prune(&mut out).unwrap();
        assert!(cache.residents.is_empty());
    }

    #[test]
    fn boundary_adds_only_new_strip_and_reverse_reuses_residents() {
        let context = context(2000, 30_000, 1972);
        let mut before = Vec::new();
        let visible = context.plan(0, 1, None, &mut before);
        let initial = before[..visible].to_vec();
        let mut after = Vec::new();
        let visible = context.plan(100, 1, None, &mut after);
        let missing: Vec<_> = after[..visible]
            .iter()
            .filter(|key| !initial.contains(key))
            .collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].y, 2048);
        // Ahead-of-scroll work already contains that strip, so a real warm crossing renders none.
        assert!(after[..visible].iter().all(|key| before.contains(key)));
        let warmed = after;
        let visible = context.plan(0, -1, None, &mut before);
        assert!(before[..visible].iter().all(|key| warmed.contains(key)));
    }

    #[test]
    fn queue_coalesces_and_rejects_obsolete_epochs_and_regions() {
        let mut keys = Vec::new();
        context(800, 10_000, 500).plan(0, 1, None, &mut keys);
        let mut queue = Queue::default();
        queue.replace(1, &keys);
        let active = queue.next().unwrap();
        queue.replace(1, &keys[1..]);
        assert!(!queue.accepts(active.0, active.1));
        assert_eq!(queue.pending.len(), keys.len() - 1);
        queue.replace(2, &keys);
        assert!(!queue.accepts(active.0, active.1));
        assert!(queue.accepts(2, active.1));
        assert_eq!(queue.next(), Some((2, keys[0])));
    }

    #[test]
    fn strips_cover_edges_short_documents_and_large_viewports_with_bounded_memory() {
        for (width, height, viewport_height) in [
            (300, 200, 500),
            (2000, 30_000, 1972),
            (2400, 30_000, 2400),
            (800, 30_000, 10_000),
        ] {
            let context = context(width, height, viewport_height);
            for y in [
                0,
                height.saturating_sub(viewport_height) / 2,
                height.saturating_sub(viewport_height),
            ] {
                for direction in [-1, 1] {
                    let mut keys = Vec::new();
                    let visible = context.plan(y, direction, None, &mut keys);
                    for key in &keys {
                        assert!(key.context.width * key.height <= STRIP_PIXELS);
                        assert!(key.context.width <= graphics::MAX_IMAGE_DIMENSION);
                        assert!(key.height <= STRIP_HEIGHT);
                        assert!(key.y + key.height <= height);
                    }
                    let mut bottom = y;
                    for key in &keys[..visible] {
                        assert!(key.y <= bottom);
                        bottom = key.y + key.height;
                    }
                    assert!(bottom >= y.saturating_add(viewport_height).min(height));
                    let pixels: u64 = keys
                        .iter()
                        .map(|key| u64::from(key.context.width) * u64::from(key.height))
                        .sum();
                    assert!(
                        pixels
                            <= u64::from(width)
                                * u64::from(
                                    viewport_height
                                        .saturating_mul(4)
                                        .saturating_add(STRIP_HEIGHT * 2)
                                )
                    );
                }
            }
        }
    }
}
