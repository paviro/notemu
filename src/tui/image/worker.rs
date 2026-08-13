//! The background decode/encode thread. Each request is dispatched onto the
//! rayon pool so an entry's images build in parallel; finished builds are
//! shipped back over a channel and folded into the cache by the runtime.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{Receiver, Sender, channel},
    thread,
};

use notema_storage::JournalStore;
use ratatui_image::{picker::Picker, protocol::Protocol};

use super::CacheKey;
use super::ascii::{self, AsciiArt};
use super::graphics;

/// How the worker turns a decoded image into something paintable. Cloned per
/// request so each rayon task owns its copy.
#[derive(Clone)]
pub(super) enum BuildMode {
    Graphics(Picker),
    Ascii,
}

/// A finished build coming back from the worker, tagged by backend.
pub(super) enum Built {
    Graphics(Protocol),
    Ascii(AsciiArt),
}

/// A decode/encode job handed to the worker thread.
pub(super) struct BuildRequest {
    pub(super) generation: u64,
    pub(super) key: CacheKey,
}

/// A finished (or failed) build coming back from the worker thread.
pub(super) struct BuildResult {
    pub(super) generation: u64,
    pub(super) key: CacheKey,
    pub(super) built: Option<Built>,
}

/// Handle to the background decode/encode thread.
pub(super) struct Worker {
    pub(super) requests: Sender<BuildRequest>,
    pub(super) results: Receiver<BuildResult>,
}

impl Worker {
    pub(super) fn spawn(store: JournalStore, mode: BuildMode) -> Self {
        let (request_tx, request_rx) = channel::<BuildRequest>();
        let (result_tx, result_rx) = channel::<BuildResult>();
        thread::spawn(move || worker_loop(store, mode, request_rx, result_tx));
        Self {
            requests: request_tx,
            results: result_rx,
        }
    }
}

/// Dispatch each request onto the rayon pool, shipping each finished build back
/// as it lands. Exits when the request channel is dropped.
fn worker_loop(
    store: JournalStore,
    mode: BuildMode,
    requests: Receiver<BuildRequest>,
    results: Sender<BuildResult>,
) {
    while let Ok(request) = requests.recv() {
        let store = store.clone();
        let mode = mode.clone();
        let results = results.clone();
        rayon::spawn(move || {
            // A panic escaping a rayon task aborts the process, skipping
            // terminal restoration — and decoding arbitrary image files is the
            // likeliest place to hit one. Report it as a failed build instead.
            let built = catch_unwind(AssertUnwindSafe(|| match &mode {
                BuildMode::Graphics(picker) => {
                    graphics::build_protocol(&store, picker, &request.key).map(Built::Graphics)
                }
                BuildMode::Ascii => ascii::build_ascii(&store, &request.key).map(Built::Ascii),
            }))
            .unwrap_or(None);
            let _ = results.send(BuildResult {
                generation: request.generation,
                key: request.key,
                built,
            });
        });
    }
}
