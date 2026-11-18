//! The HylMail design: two palettes and one `apply` call.
//!
//! The specification asks for an own design rather than a theme that follows
//! the operating system, so every colour lives here and no widget picks a
//! colour of its own.  The dark palette is the default and primary look
//! (`NFR-UX-2`); the light palette is offered so the appearance switch in the
//! settings is a real, working choice.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle,
    Theme as EguiTheme, ThemePreference, Vec2,
};
use hylmail_core::models::Theme;

/// The colours the whole interface is built from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    /// Window background behind the panels.
    pub bg: Color32,
    /// Side panel and list background.
    pub panel: Color32,
    /// Slightly lighter surface inside a panel (toolbar, list header).
    pub panel_alt: Color32,
    /// Separator lines.
    pub border: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text: dates, counts, hints.
    pub text_dim: Color32,
    /// The one accent colour, used for the selection and active elements.
    pub accent: Color32,
    /// A muted accent for inactive-but-important elements.
    pub accent_dim: Color32,
    /// Colour of an unread subject.
    pub unread: Color32,
    /// Colour of a flagged message.
    pub flagged: Color32,
    /// Positive status text.
    pub success: Color32,
    /// Errors and failures.
    pub error: Color32,
    /// Background of the selected list row.
    pub selection: Color32,
    /// Background of a hovered list row.
    pub hover: Color32,
}

/// The dark palette: the default and primary look (`NFR-UX-2`).
pub(crate) static DARK_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0x14, 0x16, 0x1B),
    panel: Color32::from_rgb(0x1B, 0x1E, 0x25),
    panel_alt: Color32::from_rgb(0x23, 0x27, 0x30),
    border: Color32::from_rgb(0x2F, 0x35, 0x41),
    text: Color32::from_rgb(0xE7, 0xEA, 0xF0),
    text_dim: Color32::from_rgb(0x98, 0xA1, 0xB2),
    accent: Color32::from_rgb(0x4C, 0x8D, 0xFF),
    accent_dim: Color32::from_rgb(0x2B, 0x50, 0x8F),
    unread: Color32::from_rgb(0x8A, 0xB8, 0xFF),
    flagged: Color32::from_rgb(0xF5, 0xC4, 0x51),
    success: Color32::from_rgb(0x6B, 0xD6, 0x8A),
    error: Color32::from_rgb(0xFF, 0x6B, 0x6B),
    selection: Color32::from_rgb(0x2C, 0x44, 0x74),
    hover: Color32::from_rgb(0x25, 0x2A, 0x36),
};

/// The light palette: the same hierarchy, high contrast on a bright surface.
pub(crate) static LIGHT_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0xF4, 0xF6, 0xF9),
    panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    panel_alt: Color32::from_rgb(0xED, 0xF0, 0xF4),
    border: Color32::from_rgb(0xD3, 0xD8, 0xDF),
    text: Color32::from_rgb(0x1B, 0x1F, 0x24),
    text_dim: Color32::from_rgb(0x5C, 0x66, 0x74),
    accent: Color32::from_rgb(0x25, 0x63, 0xEB),
    accent_dim: Color32::from_rgb(0x9D, 0xBB, 0xF5),
    unread: Color32::from_rgb(0x1D, 0x4E, 0xD8),
    flagged: Color32::from_rgb(0xB4, 0x7A, 0x0E),
    success: Color32::from_rgb(0x1F, 0x8A, 0x4C),
    error: Color32::from_rgb(0xC0, 0x39, 0x2B),
    selection: Color32::from_rgb(0xD6, 0xE4, 0xFF),
    hover: Color32::from_rgb(0xE7, 0xEE, 0xF9),
};

/// Code of the dark theme in the applied marker.
const DARK_CODE: u8 = 0;
/// Code of the light theme in the applied marker.
const LIGHT_CODE: u8 = 1;
/// Marker value meaning "nothing has been applied yet".
const NOT_APPLIED: u8 = u8::MAX;

/// The dark palette is the active one unless the light theme was applied.
static ACTIVE_DARK: AtomicBool = AtomicBool::new(true);
/// Which theme [`apply`] last installed, so [`sync`] can detect a change.
static APPLIED: AtomicU8 = AtomicU8::new(NOT_APPLIED);

/// The palette the widgets must draw with right now.
///
/// The drawing code reads colours through this function instead of a constant,
/// so the whole interface follows the theme the user picked in the settings.
#[must_use]
pub(crate) fn palette() -> &'static Palette {
    if ACTIVE_DARK.load(Ordering::Relaxed) {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    }
}

/// Installs the design for `theme` on `ctx`.
///
/// `egui` keeps a separate [`Style`](egui::Style) per [`Theme`](egui::Theme),
/// and the windowing layer reports the system theme on the first frame.
/// Writing only the current slot would therefore lose the design - including
/// the custom `Subtitle` text style, which is read back by name and panics
/// when it is missing.  So both slots are written and the preference is pinned
/// to a fixed choice; `System` resolves to the dark palette, because the
/// specification asks for an own design rather than one that follows the
/// operating system.
pub(crate) fn apply(ctx: &Context, theme: Theme) {
    let dark = is_dark(theme);
    ACTIVE_DARK.store(dark, Ordering::Relaxed);
    ctx.set_style_of(EguiTheme::Dark, style(&DARK_PALETTE, true));
    ctx.set_style_of(EguiTheme::Light, style(&LIGHT_PALETTE, false));
    ctx.set_theme(preference(theme));
    APPLIED.store(if dark { DARK_CODE } else { LIGHT_CODE }, Ordering::Relaxed);
}

/// Installs the design unless it is already in place for `theme`.
///
/// `egui` panics when a widget asks for a [`TextStyle::Name`](TextStyle::Name)
/// that the current style does not define, and a panic inside the frame loop
/// closes the window.  [`apply`] writes both theme slots so this cannot happen
/// during an ordinary theme switch; this guard covers the case where something
/// *else* replaces the whole style, and it re-applies when the user changes the
/// appearance in the settings.  It runs once per frame and costs one hash
/// lookup as long as nothing is wrong.
pub(crate) fn sync(ctx: &Context, theme: Theme) {
    let code = if is_dark(theme) {
        DARK_CODE
    } else {
        LIGHT_CODE
    };
    let styled = ctx.style().text_styles.contains_key(&subtitle());
    if !styled || APPLIED.load(Ordering::Relaxed) != code {
        apply(ctx, theme);
    }
}

/// The `egui` preference that matches the HylMail theme choice.
fn preference(theme: Theme) -> ThemePreference {
    match theme {
        Theme::Light => ThemePreference::Light,
        Theme::Dark | Theme::System => ThemePreference::Dark,
    }
}

/// Whether `theme` resolves to the dark palette.
fn is_dark(theme: Theme) -> bool {
    !matches!(theme, Theme::Light)
}

/// The complete style of one theme.
fn style(palette: &Palette, dark: bool) -> egui::Style {
    egui::Style {
        visuals: visuals(palette, dark),
        spacing: spacing(),
        text_styles: text_styles(),
        ..egui::Style::default()
    }
}

/// The widget visuals of one theme.
fn visuals(pal: &Palette, dark: bool) -> egui::Visuals {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.override_text_color = Some(pal.text);
    visuals.panel_fill = pal.panel;
    visuals.window_fill = pal.panel_alt;
    visuals.extreme_bg_color = pal.bg;
    visuals.faint_bg_color = pal.panel_alt;
    visuals.code_bg_color = pal.bg;
    visuals.hyperlink_color = pal.accent;
    visuals.warn_fg_color = pal.flagged;
    visuals.error_fg_color = pal.error;
    visuals.window_stroke = Stroke::new(1.0_f32, pal.border);
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.striped = true;
    visuals.indent_has_left_vline = true;

    visuals.selection.bg_fill = pal.selection;
    visuals.selection.stroke = Stroke::new(1.0_f32, pal.accent);

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
    }
    visuals.widgets.noninteractive.bg_fill = pal.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal.text_dim);

    visuals.widgets.inactive.weak_bg_fill = pal.panel_alt;
    visuals.widgets.inactive.bg_fill = pal.panel_alt;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, pal.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal.text);

    visuals.widgets.hovered.weak_bg_fill = pal.hover;
    visuals.widgets.hovered.bg_fill = pal.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, pal.accent_dim);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, pal.text);

    visuals.widgets.active.weak_bg_fill = pal.selection;
    visuals.widgets.active.bg_fill = pal.selection;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, pal.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, pal.text);

    visuals
}

/// Comfortable spacing for a desktop window.
fn spacing() -> egui::Spacing {
    let mut spacing = egui::Spacing {
        item_spacing: Vec2::new(8.0, 6.0),
        button_padding: Vec2::new(10.0, 5.0),
        window_margin: Margin::same(12),
        menu_margin: Margin::same(6),
        indent: 16.0,
        interact_size: Vec2::new(28.0, 24.0),
        ..egui::Spacing::default()
    };
    spacing.scroll.bar_width = 10.0;
    spacing.scroll.floating = false;
    spacing
}

/// The text style of a small subtitle inside a panel.
#[must_use]
pub(crate) fn subtitle() -> TextStyle {
    TextStyle::Name("Subtitle".into())
}

/// The one icon vocabulary of the interface.
///
/// Every glyph is covered by a font `egui` bundles - `Ubuntu-Light`,
/// `NotoEmoji-Regular` or `emoji-icon-font` - which is what keeps an icon
/// identical on Windows, macOS and Linux.  Symbols no bundled font carries
/// (the power sign `U+23FB`, the dotted circle `U+25CC`, the filled triangles
/// `U+25B2`/`U+25BC`) are therefore deliberately avoided in favour of covered
/// equivalents.  Icons are single glyphs; the surrounding spacing and label
/// stay in the drawing code.
pub(crate) mod icons {
    /// A new message.
    pub(crate) const COMPOSE: &str = "✉";
    /// Answer the sender.
    pub(crate) const REPLY: &str = "↩";
    /// Answer everyone.
    pub(crate) const REPLY_ALL: &str = "⇄";
    /// Pass the message on.
    pub(crate) const FORWARD: &str = "➡";
    /// Move to another folder.
    pub(crate) const MOVE: &str = "📁";
    /// Move to the archive.
    pub(crate) const ARCHIVE: &str = "🗄";
    /// Move to junk.
    pub(crate) const JUNK: &str = "🚫";
    /// Delete.
    pub(crate) const DELETE: &str = "🗑";
    /// Flag or unflag.
    pub(crate) const FLAG: &str = "★";
    /// Mark as read.
    pub(crate) const READ: &str = "✔";
    /// Mark as unread.
    pub(crate) const UNREAD: &str = "☐";
    /// Mark as done.
    pub(crate) const DONE: &str = "☑";
    /// Synchronise now.
    pub(crate) const SYNC: &str = "↻";
    /// Reload one account's folders.
    pub(crate) const RELOAD: &str = "↺";
    /// A status or unread dot.
    pub(crate) const DOT: &str = "●";
    /// A closed dialog or a removed row.
    pub(crate) const CLOSE: &str = "🗙";
    /// An attachment.
    pub(crate) const PAPERCLIP: &str = "📎";
    /// A calendar invitation.
    pub(crate) const CALENDAR: &str = "📅";
    /// Write one attachment to disk.
    pub(crate) const SAVE: &str = "📥";
    /// Copy the readable text.
    pub(crate) const COPY: &str = "📋";
    /// Load the blocked remote images.
    pub(crate) const IMAGES: &str = "🖼";
    /// Open the settings.
    pub(crate) const SETTINGS: &str = "⚙";
    /// Open the accounts.
    pub(crate) const ACCOUNTS: &str = "👥";
    /// Open the address book.
    pub(crate) const CONTACTS: &str = "📇";
    /// Open the rule editor.
    pub(crate) const RULES: &str = "⚖";
    /// Open the outgoing queue.
    pub(crate) const OUTBOX: &str = "📤";
    /// Leave the application.
    pub(crate) const QUIT: &str = "⏏";
    /// Sort marker, ascending.
    pub(crate) const SORT_ASC: &str = "⏶";
    /// Sort marker, descending; also an expanded section.
    pub(crate) const SORT_DESC: &str = "⏷";
    /// A collapsed account or thread.
    pub(crate) const COLLAPSED: &str = "⏵";
    /// Folder tree glyphs, one per role.
    pub(crate) const FOLDER_INBOX: &str = "📥";
    /// Sent folder.
    pub(crate) const FOLDER_SENT: &str = "📤";
    /// Drafts folder.
    pub(crate) const FOLDER_DRAFTS: &str = "📝";
    /// Archive folder.
    pub(crate) const FOLDER_ARCHIVE: &str = "🗄";
    /// Junk folder.
    pub(crate) const FOLDER_JUNK: &str = "🚫";
    /// Trash folder.
    pub(crate) const FOLDER_TRASH: &str = "🗑";
    /// Outbox folder.
    pub(crate) const FOLDER_OUTBOX: &str = "⏳";
    /// The virtual inbox of every account.
    pub(crate) const FOLDER_ALL: &str = "🌐";
    /// A user folder.
    pub(crate) const FOLDER_CUSTOM: &str = "📁";
}

/// The text hierarchy of the interface.
fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use TextStyle::{Body, Button, Heading, Monospace, Small};
    let proportional = FontFamily::Proportional;
    let monospace = FontFamily::Monospace;
    [
        (Heading, FontId::new(20.0, proportional.clone())),
        (subtitle(), FontId::new(15.0, proportional.clone())),
        (Body, FontId::new(13.5, proportional.clone())),
        (Button, FontId::new(13.5, proportional.clone())),
        (Small, FontId::new(11.0, proportional)),
        (Monospace, FontId::new(12.5, monospace)),
    ]
    .into_iter()
    .collect()
}

/// The dark [`Style`](egui::Style), for the test suite.
///
/// Only the test suite uses this: it keeps the assertions on the palette from
/// depending on a live [`Context`].
#[cfg(test)]
pub(crate) fn style_for_tests() -> egui::Style {
    style(&DARK_PALETTE, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Color32) -> f32 {
        0.2126 * f32::from(c.r()) + 0.7152 * f32::from(c.g()) + 0.0722 * f32::from(c.b())
    }

    #[test]
    fn the_dark_mode_is_on() {
        assert!(style_for_tests().visuals.dark_mode);
    }

    #[test]
    fn the_light_theme_really_is_light() {
        let style = style(&LIGHT_PALETTE, false);
        assert!(!style.visuals.dark_mode);
        let text = style.visuals.override_text_color.unwrap();
        assert!(
            luminance(style.visuals.panel_fill) - luminance(text) > 100.0,
            "the light design must keep the text legible"
        );
    }

    #[test]
    fn the_text_colour_contrasts_with_the_panel() {
        let style = style_for_tests();
        let text = style.visuals.override_text_color.unwrap();
        let panel = style.visuals.panel_fill;
        assert!(
            luminance(text) - luminance(panel) > 100.0,
            "the dark design must keep the text legible"
        );
    }

    #[test]
    fn the_accent_is_not_grey() {
        assert_ne!(DARK_PALETTE.accent.r(), DARK_PALETTE.accent.g());
        assert_ne!(LIGHT_PALETTE.accent.r(), LIGHT_PALETTE.accent.g());
    }

    #[test]
    fn the_palette_follows_the_active_theme() {
        ACTIVE_DARK.store(true, Ordering::Relaxed);
        assert_eq!(palette().panel, DARK_PALETTE.panel);
        ACTIVE_DARK.store(false, Ordering::Relaxed);
        assert_eq!(palette().panel, LIGHT_PALETTE.panel);
        ACTIVE_DARK.store(true, Ordering::Relaxed);
    }

    #[test]
    fn the_text_hierarchy_is_ordered() {
        let styles = text_styles();
        let size = |style: TextStyle| styles.get(&style).cloned().unwrap_or_default().size;
        assert!(size(TextStyle::Heading) > size(subtitle()));
        assert!(size(subtitle()) > size(TextStyle::Body));
        assert!(size(TextStyle::Body) > size(TextStyle::Small));
    }

    #[test]
    fn spacing_is_positive() {
        let spacing = spacing();
        assert!(spacing.item_spacing.x > 0.0 && spacing.item_spacing.y > 0.0);
        assert!(spacing.button_padding.x > 0.0);
        assert!(spacing.indent > 0.0);
    }

    /// Regression: the windowing layer reports the system theme on the first
    /// frame.  Before both slots were written, that swapped in `egui`'s default
    /// style and the first `RichText::text_style(subtitle())` panicked with
    /// `Failed to find Name("Subtitle") in Style::text_styles`.
    #[test]
    fn the_named_styles_survive_a_system_theme_change() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Light);
        apply(&ctx, Theme::Dark);
        assert!(ctx.style().text_styles.contains_key(&subtitle()));
        ctx.set_theme(ThemePreference::Dark);
        assert!(ctx.style().text_styles.contains_key(&subtitle()));
        assert_eq!(
            ctx.options(|options| options.theme_preference),
            ThemePreference::Dark
        );
    }

    /// The light choice installs the light preference and both named styles.
    #[test]
    fn the_light_choice_installs_the_light_preference() {
        let ctx = Context::default();
        apply(&ctx, Theme::Light);
        assert_eq!(
            ctx.options(|options| options.theme_preference),
            ThemePreference::Light
        );
        assert!(ctx.style().text_styles.contains_key(&subtitle()));
    }

    /// The frame loop repairs a style that lost the design entirely, because a
    /// missing named style panics every widget that asks for it.
    #[test]
    fn a_style_without_the_named_styles_is_repaired() {
        let ctx = Context::default();
        apply(&ctx, Theme::Dark);
        ctx.set_style(egui::Style::default());
        assert!(!ctx.style().text_styles.contains_key(&subtitle()));

        sync(&ctx, Theme::Dark);

        assert!(ctx.style().text_styles.contains_key(&subtitle()));
        assert_eq!(
            ctx.options(|options| options.theme_preference),
            ThemePreference::Dark
        );
    }

    /// Changing the appearance in the settings re-applies the design.
    #[test]
    fn a_changed_theme_is_re_applied() {
        let ctx = Context::default();
        apply(&ctx, Theme::Dark);
        sync(&ctx, Theme::Light);
        assert_eq!(
            ctx.options(|options| options.theme_preference),
            ThemePreference::Light
        );
    }

    /// An already themed context is left alone.
    #[test]
    fn a_themed_context_is_not_touched() {
        let ctx = Context::default();
        apply(&ctx, Theme::Dark);
        let before = ctx.style().text_styles.len();
        sync(&ctx, Theme::Dark);
        assert_eq!(ctx.style().text_styles.len(), before);
    }

    /// Every icon is exactly one non-blank glyph, so nothing can smuggle a
    /// second character (or an accidental space) into the vocabulary.
    #[test]
    fn every_icon_is_a_single_glyph() {
        let all = [
            icons::COMPOSE,
            icons::REPLY,
            icons::REPLY_ALL,
            icons::FORWARD,
            icons::MOVE,
            icons::ARCHIVE,
            icons::JUNK,
            icons::DELETE,
            icons::FLAG,
            icons::READ,
            icons::UNREAD,
            icons::DONE,
            icons::SYNC,
            icons::RELOAD,
            icons::DOT,
            icons::CLOSE,
            icons::PAPERCLIP,
            icons::CALENDAR,
            icons::SAVE,
            icons::COPY,
            icons::IMAGES,
            icons::SETTINGS,
            icons::ACCOUNTS,
            icons::CONTACTS,
            icons::RULES,
            icons::OUTBOX,
            icons::QUIT,
            icons::SORT_ASC,
            icons::SORT_DESC,
            icons::COLLAPSED,
            icons::FOLDER_INBOX,
            icons::FOLDER_SENT,
            icons::FOLDER_DRAFTS,
            icons::FOLDER_ARCHIVE,
            icons::FOLDER_JUNK,
            icons::FOLDER_TRASH,
            icons::FOLDER_OUTBOX,
            icons::FOLDER_ALL,
            icons::FOLDER_CUSTOM,
        ];
        for icon in all {
            assert_eq!(
                icon.chars().count(),
                1,
                "the icon {icon:?} is not a single glyph"
            );
            assert!(!icon.trim().is_empty(), "an icon must not be blank");
        }
    }
}
