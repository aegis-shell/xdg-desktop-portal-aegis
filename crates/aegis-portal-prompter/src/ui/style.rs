//! The aegis product look, mirrored locally: palette, theme factory, layout
//! metrics, and the shared style atoms for prompt surfaces. The portal build
//! graph stays independent of the Aegis repository, so the token *values*
//! are duplicated here instead of imported from `aegis-design`.
//!
//! Every dimension a dialog sets must come from [`metrics`]: heights,
//! widths, spacing, radii, font sizes, and icon sizes are design tokens, not
//! call-site literals, so controls stay on one rhythm across the dialogs.

use lens::{Color, Style, Theme};

/// The spatial and typographic tokens every prompt surface shares.
///
/// Spacing follows a 4 px grid; control heights pair so the location
/// toolbar keeps one height whether it shows breadcrumbs (28 px chips
/// centered in 36 px) or the 36 px location field.
pub mod metrics {
    // ---- spacing (4 px grid) ------------------------------------------
    /// Tightest rhythm: the gap between list rows.
    pub const SPACE_XXS: f32 = 2.0;
    /// Chip interiors and other compact padding.
    pub const SPACE_XS: f32 = 4.0;
    /// The default gap between controls in one row or section.
    pub const SPACE_S: f32 = 8.0;
    /// The root column's padding and gap.
    pub const SPACE_M: f32 = 12.0;
    /// Breathing room around small dialogs (confirm/secret root padding).
    pub const SPACE_L: f32 = 16.0;

    // ---- heights -------------------------------------------------------
    /// Single-line text fields (location, save name, folder, secret). The
    /// toolbar row is pinned to this height so swapping breadcrumbs for the
    /// location field never moves the rest of the dialog.
    pub const FIELD_HEIGHT: f32 = 36.0;
    /// Push buttons and toolbar icon buttons.
    pub const CONTROL_HEIGHT: f32 = 32.0;
    /// Directory-listing and sidebar-place rows (minimum; content can grow).
    pub const ROW_HEIGHT: f32 = 32.0;
    /// Breadcrumb chips, centered inside the FIELD_HEIGHT toolbar.
    pub const CRUMB_HEIGHT: f32 = 28.0;
    /// The text caret bar inside app-owned edit surfaces.
    pub const CARET_W: f32 = 1.5;
    pub const CARET_H: f32 = 18.0;

    // ---- widths --------------------------------------------------------
    /// The places sidebar.
    pub const SIDEBAR_WIDTH: f32 = 176.0;
    /// The quiet secondary action (Cancel).
    pub const BUTTON_WIDTH: f32 = 88.0;
    /// The default action (Open/Save/Replace/Unlock).
    pub const ACCEPT_WIDTH: f32 = 96.0;
    /// A breadcrumb chip's name is truncated to this measured width.
    pub const CRUMB_MAX_W: f32 = 160.0;

    // ---- radius --------------------------------------------------------
    /// One corner radius for every prompt control and row, matching the
    /// theme's corner radius.
    pub const RADIUS: f32 = 8.0;

    // ---- font sizes ----------------------------------------------------
    /// Body text; applied to the theme explicitly.
    pub const FONT_BODY: f32 = 14.0;
    /// The dialog heading.
    pub const FONT_TITLE: f32 = 17.0;
    /// Hints, errors, and the typeahead readout.
    pub const FONT_SMALL: f32 = 12.5;

    // ---- icons ---------------------------------------------------------
    /// Row and toolbar glyphs.
    pub const ICON: f32 = 16.0;
    /// The root breadcrumb's drive glyph.
    pub const ICON_SMALL: f32 = 14.0;
}

/// One aegis scheme's prompt-surface tokens (values mirror
/// `aegis-design`'s `Colors`, except `danger`, which has no counterpart
/// there and extends the palette locally for inline error text).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub surface: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub border: Color,
    pub hover: Color,
    pub active: Color,
    pub field: Color,
    /// Inline error text (location/folder rejection). Local extension; see
    /// the struct docs.
    pub danger: Color,
}

/// The dark aegis appearance (`Design::dark`).
pub fn dark() -> Palette {
    Palette {
        surface: Color::rgba(25, 28, 40, 255),
        text: Color::rgba(244, 246, 252, 255),
        text_muted: Color::rgba(183, 188, 207, 255),
        accent: Color::rgba(102, 156, 255, 255),
        border: Color::rgba(255, 255, 255, 42),
        hover: Color::rgba(255, 255, 255, 24),
        active: Color::rgba(102, 156, 255, 56),
        field: Color::rgba(255, 255, 255, 18),
        danger: Color::rgba(255, 124, 120, 255),
    }
}

/// The light aegis appearance (`Design::light`).
pub fn light() -> Palette {
    Palette {
        surface: Color::rgba(243, 245, 249, 255),
        text: Color::rgba(29, 33, 44, 255),
        text_muted: Color::rgba(99, 105, 123, 255),
        accent: Color::rgba(43, 101, 232, 255),
        border: Color::rgba(28, 32, 44, 32),
        hover: Color::rgba(28, 32, 44, 12),
        active: Color::rgba(43, 101, 232, 44),
        field: Color::rgba(28, 32, 44, 10),
        danger: Color::rgba(198, 47, 42, 255),
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark { self::dark() } else { light() }
}

/// The lens theme for prompt surfaces: the aegis palette over the matching
/// lens base (which supplies caret, selection, and focus-ring defaults).
pub fn theme(dark: bool) -> Theme {
    let palette = palette(dark);
    let base = if dark { Theme::dark() } else { Theme::light() };
    base.with_bg(palette.surface)
        .with_fg(palette.text)
        .with_accent(palette.accent)
        .with_border(palette.border)
        .with_hover(palette.hover)
        .with_active(palette.active)
        .with_corner_radius(metrics::RADIUS)
        .with_font_size(metrics::FONT_BODY)
}

/// The dialog heading.
pub fn title_style() -> Style {
    Style::new().with_font_size(metrics::FONT_TITLE)
}

/// Secondary (muted) text like a dialog body or hint.
pub fn muted_style(dark: bool) -> Style {
    Style::new().with_fg(palette(dark).text_muted)
}

/// Small muted text: hints and the typeahead readout.
pub fn small_muted_style(dark: bool) -> Style {
    Style::new()
        .with_fg(palette(dark).text_muted)
        .with_font_size(metrics::FONT_SMALL)
}

/// Small error text for inline rejection messages.
pub fn error_style(dark: bool) -> Style {
    Style::new()
        .with_fg(palette(dark).danger)
        .with_font_size(metrics::FONT_SMALL)
}

/// Accent text: the IME composition (preedit) inside edit surfaces.
pub fn accent_text_style(dark: bool) -> Style {
    Style::new().with_fg(palette(dark).accent)
}

/// The quiet secondary action next to the accented default. Setting only
/// `bg` lets lens derive the hover and pressed surfaces.
pub fn secondary_button_style(dark: bool) -> Style {
    Style::new().with_bg(palette(dark).hover)
}

/// The inert primary action while the dialog state is not acceptable:
/// muted text on the quiet secondary surface, no accent.
pub fn disabled_button_style(dark: bool) -> Style {
    let palette = palette(dark);
    Style::new()
        .with_bg(palette.hover)
        .with_fg(palette.text_muted)
}

/// Strip the GTK mnemonic underscore from a button label (`"_Share"` →
/// `"Share"`); lens has no mnemonic concept.
pub fn plain_label(label: &str) -> &str {
    label.trim_start_matches('_')
}
