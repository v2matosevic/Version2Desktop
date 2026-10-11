//! Spacing, radius, size and motion tokens, in egui points (1 pt = 1 px at 100 % scaling).
//! Everything sits on a 4-point grid; 2 is allowed only for hairline insets.

pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
    pub const XXXL: f32 = 48.0;
}

pub mod radius {
    /// Chips, keycaps, small inputs.
    pub const SM: u8 = 4;
    /// Buttons, list rows, inputs.
    pub const MD: u8 = 6;
    /// Cards, popovers, banners, panels.
    pub const LG: u8 = 10;
    /// Large surfaces: the queue panel, artwork, empty-state cards.
    pub const XL: u8 = 16;
    /// Floating docks and capsules.
    pub const DOCK: u8 = 22;
    pub const PILL: u8 = 255;
}

pub mod size {
    /// Compact control height (toolbars, dense lists).
    pub const CONTROL_SM: f32 = 28.0;
    /// Default control height: buttons, inputs, icon buttons.
    pub const CONTROL: f32 = 32.0;
    /// Transport buttons next to the primary play button.
    pub const CONTROL_LG: f32 = 40.0;
    /// The one prominent play/pause button per window.
    pub const PLAY: f32 = 56.0;
    /// In-window header strip below the native title bar.
    pub const HEADER: f32 = 56.0;
    /// Icon glyph inside a CONTROL-sized button.
    pub const ICON: f32 = 18.0;
    pub const ICON_SM: f32 = 16.0;
    pub const ICON_LG: f32 = 22.0;
    /// List row with title only / with a subtitle line.
    pub const ROW: f32 = 34.0;
    pub const ROW_TALL: f32 = 48.0;
    /// Seek/volume rail at rest and under the pointer.
    pub const RAIL: f32 = 4.0;
    pub const RAIL_HOT: f32 = 6.0;
    pub const THUMB: f32 = 14.0;
    /// Smallest window any app allows.
    pub const MIN_WINDOW: [f32; 2] = [480.0, 360.0];
}

/// Type sizes. Weight comes from the family: [`crate::fonts::SEMIBOLD`] for titles.
pub mod text {
    pub const CAPTION: f32 = 12.0;
    pub const BODY: f32 = 14.0;
    pub const TITLE: f32 = 16.0;
    pub const HEADING: f32 = 20.0;
    pub const DISPLAY: f32 = 28.0;
    pub const MONO: f32 = 13.0;
}

pub mod motion {
    /// Hover and press feedback.
    pub const FAST: f32 = 0.10;
    /// Panels, overlays fading in or out.
    pub const BASE: f32 = 0.18;
    /// On-video controls hide after this long without pointer movement while playing.
    pub const OSD_IDLE_SECS: f32 = 2.5;
    /// Position readout refresh while media plays and the window is visible.
    pub const PLAYING_REPAINT_MS: u64 = 250;
}
