//! Terminal-graphics backend: protocol detection and encoding a decoded image
//! into a [`Protocol`] sized to fit the viewer.

use notema_storage::JournalStore;
use notema_timing as timing;
use ratatui::layout::Size;
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};

use super::CacheKey;

/// Upper bound (px) on the longest side an image is decoded to. The effective
/// cap is the smaller of this and the terminal's pixel viewport (see
/// [`build_protocol`]); this ceiling only bites on very large displays.
const MAX_IMAGE_DIMENSION: u32 = 3000;

/// Query the terminal for a graphics protocol, applying the iTerm2 fix. `None`
/// when the query fails; the caller then falls back to ASCII art.
pub(super) fn detect_picker() -> Option<Picker> {
    let picker = Picker::from_query_stdio();
    timing::mark("image:picker-query");
    let mut picker = picker.ok()?;
    prefer_iterm2_over_kitty(&mut picker);
    Some(picker)
}

/// iTerm2 (3.5+) answers the Kitty capability query, so
/// [`Picker::from_query_stdio`] picks [`ProtocolType::Kitty`] — but
/// ratatui-image's Kitty renderer uses Unicode placeholders that iTerm2 doesn't
/// implement, so nothing appears. Force iTerm2's own inline-image protocol
/// instead. Mirrors ratatui-image's existing WezTerm/Konsole blacklist for the
/// same "answers the query but has no placeholder support" reason.
fn prefer_iterm2_over_kitty(picker: &mut Picker) {
    if picker.protocol_type() == ProtocolType::Kitty && is_iterm2() {
        picker.set_protocol_type(ProtocolType::Iterm2);
    }
}

/// Whether the host terminal is iTerm2, per the env vars it (and terminals
/// tunnelling through it via `LC_TERMINAL`) set.
fn is_iterm2() -> bool {
    std::env::var_os("TERM_PROGRAM").is_some_and(|value| value == "iTerm.app")
        || std::env::var_os("LC_TERMINAL").is_some_and(|value| value == "iTerm2")
}

/// Decrypt, decode and encode an image into a terminal protocol sized to fit
/// the viewer area. `None` if any step fails.
pub(super) fn build_protocol(
    store: &JournalStore,
    picker: &Picker,
    key: &CacheKey,
) -> Option<Protocol> {
    let bytes = store
        .read_entry_asset_bytes(&key.entry_path, &key.file_name)
        .ok()??;
    // Cap the decode before the color transform and encoding: full-res photos
    // otherwise spawn several full-frame intermediate buffers (see
    // `decode_image_with_orientation`), spiking memory far beyond the small
    // cached protocol. Cap to the device's pixel viewport (viewer cells × font
    // cell size), clamped to `MAX_IMAGE_DIMENSION` so a huge display can't blow up.
    let font = picker.font_size();
    let max_width =
        (u32::from(key.width) * u32::from(font.width.max(1))).clamp(1, MAX_IMAGE_DIMENSION);
    let max_height =
        (u32::from(key.height) * u32::from(font.height.max(1))).clamp(1, MAX_IMAGE_DIMENSION);
    let image =
        notema_storage::decode_image_with_orientation(&bytes, Some((max_width, max_height)))
            .ok()?;
    // A single whole-image protocol (one OSC 1337 for iTerm2, not one per row):
    // `Resize::Fit` fits the image into `bounds` preserving aspect ratio,
    // downscaling the already-capped image to the cell footprint.
    let bounds = Size::new(key.width.max(1), key.height.max(1));
    picker.new_protocol(image, bounds, Resize::Fit(None)).ok()
}
