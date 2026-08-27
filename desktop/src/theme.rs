//! Enclosed Space Searching Machine palette and shared widget styles.
//!
//! Both modes share one accent set: sky blue (primary), deep blue,
//! signal red (failures only), amber (pending), blossom pink, and teal
//! green (success). Light mode is the daytime look — white cards framed
//! in sky blue; dark mode keeps the same accents over a night navy.
//! Saturated accents are fills; colored *text* uses per-mode legible
//! variants from [`ui()`].

use std::sync::atomic::{AtomicBool, Ordering};

use iced::theme::Palette;
use iced::widget::container;
use iced::{Background, Border, Color, Theme};

const fn rgb8(red: u8, green: u8, blue: u8) -> Color {
    Color {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: 1.0,
    }
}

// -- Accent colors (fills and chart marks, mode-independent) --------------

/// Sky blue: the primary accent.
pub const SKY_BLUE: Color = rgb8(0x03, 0xAD, 0xF0);
/// Deep blue: secondary fills and quieter charts.
pub const DEEP_BLUE: Color = rgb8(0x01, 0x6F, 0xAD);
/// Signal red: failures and errors only.
pub const SIGNAL_RED: Color = rgb8(0xE6, 0x17, 0x37);
/// Amber: warnings and pending work.
pub const AMBER: Color = rgb8(0xFE, 0xD0, 0x37);
/// Blossom pink: symlinks and chart variety.
pub const BLOSSOM_PINK: Color = rgb8(0xF8, 0xAA, 0xC0);
/// Teal green: success, completion, published work.
pub const TEAL_GREEN: Color = rgb8(0x09, 0x84, 0x76);

/// Dark ink used on bright fills regardless of mode.
const INK: Color = rgb8(0x1C, 0x2B, 0x36);

// -- Mode-dependent surfaces and text variants -----------------------------

#[derive(Clone, Copy, Debug)]
pub struct Ui {
    pub background: Color,
    pub panel: Color,
    pub panel_highlight: Color,
    pub border: Color,
    pub text: Color,
    pub label: Color,
    /// Blue for interactive/colored text: deep in light, sky in dark.
    pub link: Color,
    /// Legible gold for pending-state text.
    pub gold_text: Color,
    /// Success green tuned for text on this mode's ground.
    pub green_text: Color,
}

const LIGHT: Ui = Ui {
    background: rgb8(0xF0, 0xF7, 0xFC),
    panel: rgb8(0xFF, 0xFF, 0xFF),
    panel_highlight: rgb8(0xDD, 0xEF, 0xFA),
    border: rgb8(0xD4, 0xE5, 0xF0),
    text: INK,
    label: rgb8(0x5E, 0x78, 0x89),
    link: DEEP_BLUE,
    gold_text: rgb8(0xA8, 0x7D, 0x00),
    green_text: TEAL_GREEN,
};

const DARK: Ui = Ui {
    background: rgb8(0x0B, 0x14, 0x1E),
    panel: rgb8(0x12, 0x1E, 0x2B),
    panel_highlight: rgb8(0x1C, 0x2E, 0x40),
    border: rgb8(0x21, 0x34, 0x4A),
    text: rgb8(0xE8, 0xF2, 0xFA),
    label: rgb8(0x8A, 0xA2, 0xB5),
    link: SKY_BLUE,
    gold_text: rgb8(0xFF, 0xD7, 0x5E),
    green_text: rgb8(0x2F, 0xB5, 0xA0),
};

static DARK_MODE: AtomicBool = AtomicBool::new(false);

pub fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

pub fn set_dark(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
}

/// The active mode's surface and text colors.
pub fn ui() -> Ui {
    if is_dark() { DARK } else { LIGHT }
}

pub fn theme() -> Theme {
    let ui = ui();
    Theme::custom(
        if is_dark() {
            "Enclosed Space Searching Machine Dark"
        } else {
            "Enclosed Space Searching Machine"
        },
        Palette {
            background: ui.background,
            text: ui.text,
            primary: SKY_BLUE,
            success: TEAL_GREEN,
            warning: AMBER,
            danger: SIGNAL_RED,
        },
    )
}

/// Black-or-white text for whatever fill it sits on.
pub fn on_color(background: Color) -> Color {
    let luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
    if luminance > 0.62 { INK } else { Color::WHITE }
}

/// Chip and bar fills for a phase.
pub fn phase_color(phase: &str) -> Color {
    match phase {
        "complete" => TEAL_GREEN,
        "incomplete" => AMBER,
        "fatal" | "disconnected" => SIGNAL_RED,
        _ => SKY_BLUE,
    }
}

/// Text color for a directory state.
pub fn state_color(state: &str) -> Color {
    match state {
        "published" => ui().green_text,
        "failed" => SIGNAL_RED,
        _ => ui().gold_text,
    }
}

/// Rounded card with the mode's panel background and a soft outline.
pub fn panel(_theme: &Theme) -> container::Style {
    let ui = ui();
    container::Style {
        background: Some(Background::Color(ui.panel)),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: ui.border,
        },
        ..container::Style::default()
    }
}

/// A flat colored swatch, used for bars and badges.
pub fn swatch(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            radius: 2.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}
