use crate::color::blend;
use crate::color::is_light;
use crate::terminal_palette::best_color;
use crate::terminal_palette::default_bg;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

pub fn accent_color() -> Color {
    accent_color_for(default_bg())
}

pub fn accent_color_for(terminal_bg: Option<(u8, u8, u8)>) -> Color {
    match terminal_bg {
        Some(bg) => {
            let target = if is_light(bg) {
                (0, 112, 163)
            } else {
                (110, 231, 255)
            };
            best_color(target)
        }
        None => Color::Cyan,
    }
}

pub fn accent_style() -> Style {
    Style::default()
        .fg(accent_color())
        .add_modifier(Modifier::BOLD)
}

pub fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub fn proposed_plan_style() -> Style {
    proposed_plan_style_for(default_bg())
}

/// Returns the style for a user-authored message using the provided terminal background.
pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => Style::default(),
    }
}

pub fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(proposed_plan_bg(bg)),
        None => Style::default(),
    }
}

#[allow(clippy::disallowed_methods)]
pub fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 112, 163), 0.08)
    } else {
        ((56, 189, 248), 0.18)
    };
    best_color(blend(top, terminal_bg, alpha))
}

#[allow(clippy::disallowed_methods)]
pub fn proposed_plan_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((5, 150, 105), 0.08)
    } else {
        ((52, 211, 153), 0.16)
    };
    best_color(blend(top, terminal_bg, alpha))
}
