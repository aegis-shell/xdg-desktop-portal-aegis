//! The aegis product look, mirrored locally: palette, theme factory, and the
//! shared style atoms for prompt surfaces. The portal build graph stays
//! independent of the Aegis repository, so the token *values* are duplicated
//! here instead of imported from `aegis-design`.

use lens::{Color, Style, Theme};

/// One aegis scheme's prompt-surface tokens (values mirror
/// `aegis-design`'s `Colors`).
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
        .with_corner_radius(8.0)
}

/// The dialog heading.
pub fn title_style() -> Style {
    Style::new().with_font_size(17.0)
}

/// Secondary (muted) text like a dialog body or hint.
pub fn muted_style(dark: bool) -> Style {
    Style::new().with_fg(palette(dark).text_muted)
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
