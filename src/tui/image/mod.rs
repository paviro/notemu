//! Terminal image rendering for the fullscreen image viewer.
//!
//! Uses a real graphics protocol (Kitty, iTerm2, or sixel) when the terminal
//! supports one, else falls back to theme-independent half-block colour cells.
//! Encrypted assets are decrypted straight into memory — plaintext is never
//! written to disk.
//!
//! Decode/encode is expensive, so it runs on a background worker ([`worker`])
//! that fans builds across a rayon pool; the render path only paints already-
//! built images out of a cache and repaints once the worker reports back. The
//! cache is warmed ([`ImageRuntime::warm`]) when the image viewer opens and
//! dropped ([`ImageRuntime::clear`]) when the entry closes, so viewer
//! navigation is instant.

mod ascii;
mod graphics;
mod refs;
mod worker;

pub(crate) use refs::{entry_images, sole_image_ref};

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use notema_storage::JournalStore;
use ratatui::{
    Frame,
    layout::{Rect, Size},
    widgets::Paragraph,
};
use ratatui_image::{Image, picker::ProtocolType, protocol::Protocol};

use ascii::AsciiArt;
use worker::{BuildMode, BuildRequest, Built, Worker};

/// Cell size the viewer renders an image into. Precompute and the viewer both
/// use this so their [`CacheKey`]s match exactly.
pub(crate) fn viewer_image_size(area: Rect) -> Size {
    Size::new(
        area.width.saturating_sub(2).max(1),
        area.height.saturating_sub(2).max(1),
    )
}

/// Build state of an image the viewer wants to show this frame.
pub(crate) enum ImageStatus {
    /// Built and ready to paint with [`ImageRuntime::render`].
    Ready,
    /// Still building on the worker; caller shows a loading notice.
    Loading,
    /// Build failed or no backend active; caller shows a text notice.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageAsset {
    pub(crate) entry_path: PathBuf,
    pub(crate) file_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WarmRequest {
    pub(crate) assets: Vec<ImageAsset>,
    pub(crate) size: Size,
}

pub(crate) struct ImageRuntime {
    backend: Backend,
    cache: RefCell<HashMap<CacheKey, CacheState>>,
    /// Bumped on clear so stale in-flight builds are dropped when they arrive.
    generation: RefCell<u64>,
    worker: Option<Worker>,
}

/// Rendering backend the runtime settled on after querying the terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// No usable backend (before `detect`, or if the worker can't start).
    Disabled,
    /// A terminal graphics protocol (Kitty, iTerm2, or sixel).
    Graphics,
    /// Colored ASCII art fallback.
    Ascii,
}

/// An asset (entry + file) rendered at a specific cell size.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct CacheKey {
    pub(super) entry_path: PathBuf,
    pub(super) file_name: String,
    pub(super) width: u16,
    pub(super) height: u16,
}

enum CacheState {
    Loading,
    ReadyGraphics(Rc<Protocol>),
    ReadyAscii(Rc<AsciiArt>),
    Failed,
}

/// `Rc`-backed handle to a finished build, cloned out of the cache so the
/// borrow can be dropped before painting.
enum Ready {
    Graphics(Rc<Protocol>),
    Ascii(Rc<AsciiArt>),
}

impl Default for ImageRuntime {
    fn default() -> Self {
        Self {
            backend: Backend::Disabled,
            cache: RefCell::new(HashMap::new()),
            generation: RefCell::new(0),
            worker: None,
        }
    }
}

impl ImageRuntime {
    /// Detect terminal graphics support and pick a backend. Detection queries
    /// the terminal, so this must run with the terminal already in raw mode.
    /// The worker gets its own store clone so it can decrypt assets.
    pub(crate) fn detect(store: &JournalStore) -> Self {
        let picker = graphics::detect_picker();
        let graphics = picker.as_ref().is_some_and(|picker| {
            matches!(
                picker.protocol_type(),
                ProtocolType::Kitty | ProtocolType::Sixel | ProtocolType::Iterm2
            )
        });

        let (backend, mode) = match (graphics, picker) {
            (true, Some(picker)) => (Backend::Graphics, BuildMode::Graphics(picker)),
            _ => (Backend::Ascii, BuildMode::Ascii),
        };

        Self {
            backend,
            cache: RefCell::new(HashMap::new()),
            generation: RefCell::new(0),
            worker: Some(Worker::spawn(store.clone(), mode)),
        }
    }

    /// Whether real images can be rendered (vs. a text notice).
    pub(crate) fn enabled(&self) -> bool {
        !matches!(self.backend, Backend::Disabled)
    }

    /// Whether the active backend is a terminal graphics protocol. Graphics
    /// paints with `skip` cells that ratatui's diff leaves alone, so the caller
    /// must force a full clear when an overlay over an image closes; the ASCII
    /// backend paints ordinary cells and needs no such special-casing.
    pub(crate) fn uses_graphics(&self) -> bool {
        matches!(self.backend, Backend::Graphics)
    }

    /// Fold finished builds into the cache. Returns whether anything changed (so
    /// the caller can redraw). Results from a superseded generation are dropped.
    pub(crate) fn poll_results(&self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        let mut changed = false;
        while let Ok(result) = worker.results.try_recv() {
            if result.generation != *self.generation.borrow() {
                continue;
            }
            let mut cache = self.cache.borrow_mut();
            if let Some(entry) = cache.get_mut(&result.key) {
                *entry = match result.built {
                    Some(Built::Graphics(proto)) => CacheState::ReadyGraphics(Rc::new(proto)),
                    Some(Built::Ascii(art)) => CacheState::ReadyAscii(Rc::new(art)),
                    None => CacheState::Failed,
                };
                changed = true;
            }
        }
        changed
    }

    /// Whether any image is still building, so the caller can poll more eagerly.
    pub(crate) fn has_pending(&self) -> bool {
        self.cache
            .borrow()
            .values()
            .any(|state| matches!(state, CacheState::Loading))
    }

    /// Drop every cached build and bump the generation so in-flight builds are
    /// ignored when they return. Called when the viewer closes and after an edit.
    pub(crate) fn clear(&self) {
        self.cache.borrow_mut().clear();
        *self.generation.borrow_mut() += 1;
    }

    /// Kick off background builds for every image in `assets` at `size` so the
    /// viewer finds them ready. Run when the viewer is opened (and on resize).
    pub(crate) fn warm(&self, assets: &[ImageAsset], size: Size) {
        for asset in assets {
            self.start(asset, size);
        }
    }

    fn start(&self, asset: &ImageAsset, area: Size) {
        if !self.enabled() {
            return;
        }
        let key = self.key(asset, area);
        if self.cache.borrow().contains_key(&key) {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let request = BuildRequest {
            generation: *self.generation.borrow(),
            key: key.clone(),
        };
        if worker.requests.send(request).is_ok() {
            self.cache.borrow_mut().insert(key, CacheState::Loading);
        }
    }

    /// Report the build state of `asset` at `area` without changing the cache.
    pub(crate) fn status(&self, asset: &ImageAsset, area: Size) -> ImageStatus {
        if !self.enabled() {
            return ImageStatus::Unavailable;
        }
        let key = self.key(asset, area);

        if let Some(state) = self.cache.borrow().get(&key) {
            return match state {
                CacheState::ReadyGraphics(_) | CacheState::ReadyAscii(_) => ImageStatus::Ready,
                CacheState::Loading => ImageStatus::Loading,
                CacheState::Failed => ImageStatus::Unavailable,
            };
        }

        ImageStatus::Unavailable
    }

    /// Paint a previously reserved image centered within `area`. A no-op unless
    /// the image has finished building at this `area`'s size.
    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, asset: &ImageAsset) {
        let key = self.key(asset, Size::new(area.width, area.height));
        // Clone the `Rc` handle out so the cache borrow is released before render.
        let ready = self.cache.borrow().get(&key).and_then(|state| match state {
            CacheState::ReadyGraphics(proto) => Some(Ready::Graphics(proto.clone())),
            CacheState::ReadyAscii(art) => Some(Ready::Ascii(art.clone())),
            _ => None,
        });

        match ready {
            Some(Ready::Graphics(proto)) => {
                // Aspect-preserving fit may letterbox — center the rendered cells.
                // `Image` draws at the top-left of its area and skips rendering if
                // the protocol is larger than it, so pass an exact centered rect.
                let rendered = proto.size();
                let rect = Rect {
                    x: area.x + area.width.saturating_sub(rendered.width) / 2,
                    y: area.y + area.height.saturating_sub(rendered.height) / 2,
                    width: rendered.width.min(area.width),
                    height: rendered.height.min(area.height),
                };
                frame.render_widget(Image::new(&proto), rect);
            }
            Some(Ready::Ascii(art)) => {
                // Already sized to fit inside `area`; center it.
                let x = area.x + area.width.saturating_sub(art.cols) / 2;
                let y = area.y + area.height.saturating_sub(art.rows) / 2;
                let rect = Rect {
                    x,
                    y,
                    width: art.cols.min(area.width),
                    height: art.rows.min(area.height),
                };
                frame.render_widget(Paragraph::new(art.text.clone()), rect);
            }
            None => {}
        }
    }

    fn key(&self, asset: &ImageAsset, area: Size) -> CacheKey {
        CacheKey {
            entry_path: asset.entry_path.clone(),
            file_name: asset.file_name.clone(),
            width: area.width.max(1),
            height: area.height.max(1),
        }
    }
}
