//! glance's design tokens.
//!
//! Colours follow the Discord design language (dark greys, blurple accent,
//! green "speaking" ring, red "LIVE"/leave). The component theme lives in
//! `assets/themes/glance.json` and is applied to gpui-kit at startup; the
//! [`Palette`] covers app-specific surfaces that the component theme has no
//! slot for (server rail, user panel, status colours).

use std::rc::Rc;

use gpui_kit::component::theme::{Theme, ThemeConfig, ThemeMode, ThemeSet};
use gpui_kit::{App, Hsla, rgb, rgba};

/// The bundled theme set (dark + light).
pub const THEMES_JSON: &str = include_str!("../assets/themes/glance.json");

pub const DARK_THEME_NAME: &str = "Glance Dark";
pub const LIGHT_THEME_NAME: &str = "Glance Light";

/// Parse the bundled theme set and return (dark, light).
pub fn bundled_themes() -> anyhow::Result<(ThemeConfig, ThemeConfig)> {
    let set: ThemeSet = serde_json::from_str(THEMES_JSON)?;
    let mut dark = None;
    let mut light = None;
    for theme in set.themes {
        match theme.name.as_ref() {
            DARK_THEME_NAME => dark = Some(theme),
            LIGHT_THEME_NAME => light = Some(theme),
            _ => {}
        }
    }
    match (dark, light) {
        (Some(d), Some(l)) => Ok((d, l)),
        _ => anyhow::bail!(
            "bundled theme set must contain '{DARK_THEME_NAME}' and '{LIGHT_THEME_NAME}'"
        ),
    }
}

/// Install glance's themes into gpui-kit and switch to `mode`.
/// Call after `gpui_kit::init`.
pub fn init(mode: ThemeMode, cx: &mut App) {
    let (dark, light) = bundled_themes().expect("bundled theme is valid");
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.dark_theme = Rc::new(dark);
        theme.light_theme = Rc::new(light);
    });
    // Re-apply so the new configs take effect for the active mode.
    Theme::change(mode, None, cx);
}

/// App-specific colours, resolved for the active light/dark mode.
// Some slots are only used by later milestones (stage, statuses).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Leftmost rail (server list in Discord).
    pub bg_rail: Hsla,
    /// Channel/contacts sidebar.
    pub bg_sidebar: Hsla,
    /// User panel at the bottom of the sidebar.
    pub bg_user_panel: Hsla,
    /// Main content area.
    pub bg_content: Hsla,
    /// Video stage behind tiles.
    pub bg_stage: Hsla,
    /// Hovered row/button.
    pub bg_hover: Hsla,
    /// Selected row.
    pub bg_selected: Hsla,
    /// Text input fill.
    pub bg_input: Hsla,
    pub text_normal: Hsla,
    pub text_muted: Hsla,
    pub text_header: Hsla,
    pub interactive: Hsla,
    pub interactive_hover: Hsla,
    pub brand: Hsla,
    pub brand_hover: Hsla,
    /// Online status, speaking ring.
    pub green: Hsla,
    /// LIVE badge, leave/hang-up, muted icon.
    pub red: Hsla,
    pub red_hover: Hsla,
    /// Away/idle status.
    pub yellow: Hsla,
    /// Someone you muted for yourself (apart from red, their own mute).
    pub muted_by_you: Hsla,
    /// Offline/invisible status.
    pub gray: Hsla,
    /// Context menus (floating, darker than everything under them).
    pub bg_menu: Hsla,
    /// Context menu outline and separators.
    pub menu_border: Hsla,
    /// Behind the full-window image viewer.
    pub lightbox_backdrop: Hsla,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            bg_rail: rgb(0x1e1f22).into(),
            bg_sidebar: rgb(0x2b2d31).into(),
            bg_user_panel: rgb(0x232428).into(),
            bg_content: rgb(0x313338).into(),
            bg_stage: rgb(0x000000).into(),
            bg_hover: rgb(0x35373c).into(),
            bg_selected: rgb(0x404249).into(),
            bg_input: rgb(0x1e1f22).into(),
            text_normal: rgb(0xdbdee1).into(),
            text_muted: rgb(0x949ba4).into(),
            text_header: rgb(0xf2f3f5).into(),
            interactive: rgb(0xb5bac1).into(),
            interactive_hover: rgb(0xdbdee1).into(),
            brand: rgb(0x5865f2).into(),
            brand_hover: rgb(0x4752c4).into(),
            green: rgb(0x23a55a).into(),
            red: rgb(0xda373c).into(),
            red_hover: rgb(0xa12828).into(),
            yellow: rgb(0xf0b232).into(),
            muted_by_you: rgb(0xe8732c).into(),
            gray: rgb(0x80848e).into(),
            bg_menu: rgb(0x111214).into(),
            menu_border: rgb(0x2b2d31).into(),
            lightbox_backdrop: rgba(0x000000e0).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            bg_rail: rgb(0xe3e5e8).into(),
            bg_sidebar: rgb(0xf2f3f5).into(),
            bg_user_panel: rgb(0xebedef).into(),
            bg_content: rgb(0xffffff).into(),
            bg_stage: rgb(0x000000).into(),
            bg_hover: rgb(0xe0e1e5).into(),
            bg_selected: rgb(0xd7d9dc).into(),
            bg_input: rgb(0xe3e5e8).into(),
            text_normal: rgb(0x313338).into(),
            text_muted: rgb(0x5c5e66).into(),
            text_header: rgb(0x060607).into(),
            interactive: rgb(0x4e5058).into(),
            interactive_hover: rgb(0x313338).into(),
            brand: rgb(0x5865f2).into(),
            brand_hover: rgb(0x4752c4).into(),
            green: rgb(0x23a55a).into(),
            red: rgb(0xda373c).into(),
            red_hover: rgb(0xa12828).into(),
            yellow: rgb(0xf0b232).into(),
            muted_by_you: rgb(0xe8732c).into(),
            gray: rgb(0x80848e).into(),
            bg_menu: rgb(0xffffff).into(),
            menu_border: rgb(0xe3e5e8).into(),
            lightbox_backdrop: rgba(0x000000cc).into(),
        }
    }

    /// The palette for the current theme mode.
    pub fn current(cx: &App) -> Self {
        if Theme::global(cx).is_dark() {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_themes_parse_with_both_modes() {
        let (dark, light) = bundled_themes().unwrap();
        assert!(dark.mode.is_dark());
        assert!(!light.mode.is_dark());
    }
}
