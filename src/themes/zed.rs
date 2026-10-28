//! Zed theme converter: turns Zed theme-family files into GPUI Kit themes.

use crate::color::{self, Rgb};
use gpui_kit::component::ThemeConfig;
use gpui_kit::{Hsla, Rgba};
use serde_json::{Map, Value};

/// Theme file formats found in the user themes directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeFileFormat {
    /// A GPUI Kit `ThemeSet`.
    Kit,
    /// A Zed theme family.
    Zed,
}

/// Errors raised while converting one Zed theme.
#[derive(Debug, thiserror::Error)]
pub enum ZedThemeError {
    /// The theme entry is not a JSON object.
    #[error("theme entry is not an object")]
    NotAnObject,
    /// The theme has no string `name`.
    #[error("theme has no name")]
    MissingName,
    /// The theme has no `style` object.
    #[error("theme {theme:?} has no style object")]
    MissingStyle {
        /// Theme name.
        theme: String,
    },
    /// `appearance` is missing or neither `light` nor `dark`.
    #[error("theme {theme:?} has a missing or invalid appearance")]
    BadAppearance {
        /// Theme name.
        theme: String,
    },
    /// The assembled Kit theme could not be deserialized.
    #[error("theme {theme:?} is not a valid Kit theme: {source}")]
    Invalid {
        /// Theme name.
        theme: String,
        /// Underlying deserialization error.
        source: serde_json::Error,
    },
}

/// Kit color key <- first present Zed style key.
const COLOR_MAP: &[(&str, &[&str])] = &[
    ("background", &["editor.background", "background"]),
    ("foreground", &["text"]),
    ("border", &["border"]),
    ("input.border", &["border"]),
    ("ring", &["border.focused"]),
    (
        "muted.background",
        &["element.background", "surface.background"],
    ),
    ("muted.foreground", &["text.muted"]),
    (
        "accent.background",
        &["ghost_element.hover", "element.hover"],
    ),
    ("accent.foreground", &["text"]),
    (
        "popover.background",
        &["elevated_surface.background", "surface.background"],
    ),
    ("popover.foreground", &["text"]),
    ("primary.background", &["text.accent", "players[0].cursor"]),
    ("secondary.background", &["element.background"]),
    ("secondary.hover.background", &["element.hover"]),
    ("secondary.active.background", &["element.active"]),
    ("secondary.foreground", &["text"]),
    ("selection.background", &["players[0].selection"]),
    ("caret", &["players[0].cursor"]),
    ("sidebar.background", &["panel.background"]),
    ("sidebar.foreground", &["text"]),
    ("sidebar.border", &["border.variant", "border"]),
    ("sidebar.accent.background", &["ghost_element.hover"]),
    ("sidebar.accent.foreground", &["text"]),
    ("list.background", &["panel.background"]),
    ("list.hover.background", &["ghost_element.hover"]),
    (
        "list.active.background",
        &["ghost_element.selected", "element.selected"],
    ),
    ("list.active.border", &["border.selected", "border.focused"]),
    ("tab_bar.background", &["tab_bar.background"]),
    ("tab.background", &["tab.inactive_background"]),
    ("tab.active.background", &["tab.active_background"]),
    ("tab.foreground", &["text.muted"]),
    ("tab.active.foreground", &["text"]),
    ("title_bar.background", &["title_bar.background"]),
    ("title_bar.border", &["border.variant"]),
    ("status_bar.background", &["status_bar.background"]),
    ("status_bar.border", &["border.variant"]),
    ("table.background", &["editor.background"]),
    ("table.head.background", &["panel.background"]),
    ("table.head.foreground", &["text.muted"]),
    ("table.hover.background", &["ghost_element.hover"]),
    ("table.active.background", &["ghost_element.selected"]),
    ("table.active.border", &["border.selected"]),
    ("table.row.border", &["border.variant"]),
    ("scrollbar.background", &["scrollbar.track.background"]),
    (
        "scrollbar.thumb.background",
        &["scrollbar.thumb.background"],
    ),
    (
        "scrollbar.thumb.hover.background",
        &["scrollbar.thumb.hover_background"],
    ),
    ("danger.background", &["error"]),
    ("warning.background", &["warning"]),
    ("success.background", &["success"]),
    ("info.background", &["info"]),
    ("link", &["text.accent"]),
    ("link.hover", &["link_text.hover"]),
    ("drop_target.background", &["drop_target.background"]),
    ("drag.border", &["border.focused"]),
    ("window.border", &["border"]),
];

/// Kit foreground key <- Kit background key it must contrast with.
const READABLE_PAIRS: &[(&str, &str)] = &[
    ("primary.foreground", "primary.background"),
    ("danger.foreground", "danger.background"),
    ("warning.foreground", "warning.background"),
    ("success.foreground", "success.background"),
    ("info.foreground", "info.background"),
];

const HIGHLIGHT_KEYS: &[&str] = &[
    "editor.background",
    "editor.foreground",
    "editor.active_line.background",
    "editor.line_number",
    "editor.active_line_number",
    "editor.invisible",
    "editor.gutter.background",
];

const STATUS_KEYS: &[&str] = &["error", "warning", "info", "success", "hint"];
const WEIGHTS: [u16; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];

/// Detects the format of a parsed theme file.
///
/// Zed when the top-level `themes` array has an object with a `style` object
/// and an `appearance` string; Kit otherwise.
#[must_use]
pub fn detect_format(value: &Value) -> ThemeFileFormat {
    let is_zed = value
        .get("themes")
        .and_then(Value::as_array)
        .is_some_and(|themes| {
            themes.iter().any(|theme| {
                theme.get("style").is_some_and(Value::is_object)
                    && theme.get("appearance").is_some_and(Value::is_string)
            })
        });
    if is_zed {
        ThemeFileFormat::Zed
    } else {
        ThemeFileFormat::Kit
    }
}

/// Converts every theme of a Zed family, one result per theme.
///
/// # Errors
/// Each element is an error when that theme has no name or style, a bad
/// `appearance`, or cannot be deserialized as a Kit theme; siblings are
/// unaffected.
#[must_use]
pub fn convert_family(value: &Value) -> Vec<Result<ThemeConfig, ZedThemeError>> {
    value
        .get("themes")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |themes| {
            themes.iter().map(convert_theme).collect()
        })
}

fn convert_theme(theme: &Value) -> Result<ThemeConfig, ZedThemeError> {
    let theme = theme.as_object().ok_or(ZedThemeError::NotAnObject)?;
    let name = theme
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ZedThemeError::MissingName)?;
    let owned = || name.to_owned();
    let Some(mode @ ("light" | "dark")) = theme.get("appearance").and_then(Value::as_str) else {
        return Err(ZedThemeError::BadAppearance { theme: owned() });
    };
    let style = theme
        .get("style")
        .and_then(Value::as_object)
        .ok_or_else(|| ZedThemeError::MissingStyle { theme: owned() })?;
    let kit = serde_json::json!({
        "name": name,
        "mode": mode,
        "colors": convert_colors(style),
        "highlight": convert_highlight(style),
    });
    serde_json::from_value(kit).map_err(|source| ZedThemeError::Invalid {
        theme: owned(),
        source,
    })
}

fn style_color<'a>(style: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    let value = match key.strip_prefix("players[0].") {
        Some(field) => style.get("players")?.get(0)?.get(field)?,
        None => style.get(key)?,
    };
    value.as_str().filter(|text| Rgba::try_from(*text).is_ok())
}

fn to_rgb(text: &str) -> Option<Rgb> {
    let rgba = Rgba::try_from(text).ok()?;
    Some(Rgb::from_hsla(Hsla::from(rgba)))
}

fn convert_colors(style: &Map<String, Value>) -> Map<String, Value> {
    let mut colors = Map::new();
    for (kit_key, zed_keys) in COLOR_MAP {
        if let Some(text) = zed_keys.iter().find_map(|key| style_color(style, key)) {
            colors.insert((*kit_key).to_owned(), Value::from(text));
        }
    }
    for (foreground, background) in READABLE_PAIRS {
        let derived = colors
            .get(*background)
            .and_then(Value::as_str)
            .and_then(to_rgb);
        if let Some(bg) = derived {
            colors.insert(
                (*foreground).to_owned(),
                Value::from(color::readable_foreground(bg).to_hex()),
            );
        }
    }
    if let Some(primary) = colors
        .get("primary.background")
        .and_then(Value::as_str)
        .and_then(to_rgb)
    {
        let hover = Value::from(color::hover_variant(primary).to_hex());
        colors.insert("primary.hover.background".to_owned(), hover.clone());
        colors.insert("primary.active.background".to_owned(), hover);
    }
    colors
}

fn convert_highlight(style: &Map<String, Value>) -> Map<String, Value> {
    let status = STATUS_KEYS.iter().flat_map(|key| {
        [
            (*key).to_owned(),
            format!("{key}.background"),
            format!("{key}.border"),
        ]
    });
    let keys = HIGHLIGHT_KEYS
        .iter()
        .map(|key| (*key).to_owned())
        .chain(status);
    let mut highlight = Map::new();
    for key in keys {
        if let Some(text) = style_color(style, &key) {
            highlight.insert(key, Value::from(text));
        }
    }
    let syntax = style
        .get("syntax")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(name, entry)| {
                    Some((name.clone(), sanitize_syntax(entry.as_object()?)))
                })
                .collect()
        });
    highlight.insert(
        "syntax".to_owned(),
        Value::Object(syntax.unwrap_or_default()),
    );
    highlight
}

fn sanitize_syntax(entry: &Map<String, Value>) -> Value {
    let color = entry
        .get("color")
        .and_then(Value::as_str)
        .filter(|text| Rgba::try_from(*text).is_ok());
    let font_style = match entry.get("font_style").and_then(Value::as_str) {
        Some("oblique" | "italic") => Value::from("italic"),
        Some(kept @ ("normal" | "underline")) => Value::from(kept),
        Some(_) | None => Value::Null,
    };
    serde_json::json!({
        "color": color,
        "font_style": font_style,
        "font_weight": entry.get("font_weight").map_or(Value::Null, sanitize_weight),
    })
}

/// Rounds a numeric weight to the nearest 100 within `100..=900`; others become null.
fn sanitize_weight(value: &Value) -> Value {
    let Some(weight) = value.as_f64() else {
        return Value::Null;
    };
    let target = (weight / 100.0).round().clamp(1.0, 9.0) * 100.0;
    WEIGHTS
        .iter()
        .find(|candidate| (f64::from(**candidate) - target).abs() < 0.5)
        .map_or(Value::Null, |candidate| Value::from(*candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::{ActiveTheme as _, Theme};
    use gpui_kit::{Hsla, Rgba, TestAppContext};
    use std::rc::Rc;

    const SAMPLE: &str = include_str!("fixtures/zed-sample.json");
    const BROKEN: &str = include_str!("fixtures/zed-broken.json");

    fn load(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn sample_configs() -> Vec<ThemeConfig> {
        convert_family(&load(SAMPLE))
            .into_iter()
            .map(Result::unwrap)
            .collect()
    }

    fn colors(config: &ThemeConfig) -> Value {
        serde_json::to_value(config).unwrap()["colors"].clone()
    }

    fn syntax(config: &ThemeConfig, name: &str) -> Value {
        serde_json::to_value(config).unwrap()["highlight"]["syntax"][name].clone()
    }

    #[test]
    fn detects_formats() {
        // Given: a Kit snippet and a Zed snippet.
        let kit = serde_json::json!({ "themes": [{ "name": "k", "mode": "dark", "colors": {} }] });
        let zed = load(SAMPLE);
        // When / Then: each is recognised.
        assert_eq!(detect_format(&kit), ThemeFileFormat::Kit);
        assert_eq!(detect_format(&zed), ThemeFileFormat::Zed);
        assert_eq!(detect_format(&serde_json::json!({})), ThemeFileFormat::Kit);
    }

    #[test]
    fn sample_converts_to_two_themes_with_modes() {
        // Given / When: the sample family is converted.
        let configs = sample_configs();
        // Then: one light and one dark config.
        let modes: Vec<bool> = configs.iter().map(|c| c.mode.is_dark()).collect();
        assert_eq!(modes, [false, true]);
    }

    #[test]
    fn background_is_the_editor_background() {
        // Given / When: the sample is converted.
        let configs = sample_configs();
        // Then: Kit background is the editor background, not the surface one.
        assert_eq!(colors(&configs[0])["background"], "#f6f1e8");
        assert_eq!(colors(&configs[1])["background"], "#14161b");
    }

    #[test]
    fn primary_foreground_is_readable_on_accent() {
        // Given: the dark theme with text.accent #6aa9e9.
        let configs = sample_configs();
        let accent = Rgb::parse_hex("#6aa9e9").unwrap();
        // When: reading the computed primary colors.
        let colors = colors(&configs[1]);
        // Then: the foreground is the readable one and hover differs.
        assert_eq!(
            colors["primary.foreground"],
            color::readable_foreground(accent).to_hex()
        );
        assert_eq!(
            colors["primary.hover.background"],
            color::hover_variant(accent).to_hex()
        );
    }

    #[test]
    fn oblique_becomes_italic() {
        // Given / When: the light theme has an oblique comment.
        let configs = sample_configs();
        // Then: it is italic.
        assert_eq!(syntax(&configs[0], "comment")["font_style"], "italic");
    }

    #[test]
    fn unsupported_syntax_values_are_sanitized() {
        // Given / When: weights 450 and "bold", style "wavy".
        let configs = sample_configs();
        // Then: 450 rounds to 500; non-numeric weight and unknown style are null.
        assert_eq!(syntax(&configs[0], "keyword")["font_weight"], 500);
        assert!(syntax(&configs[0], "string")["font_style"].is_null());
        assert!(syntax(&configs[0], "string")["font_weight"].is_null());
    }

    #[test]
    fn missing_and_null_keys_are_absent() {
        // Given / When: the light theme has a null border.focused and no tab colors.
        let configs = sample_configs();
        let colors = colors(&configs[0]);
        // Then: derived keys are absent while present ones remain.
        assert!(colors["ring"].is_null());
        assert!(colors["tab.active.background"].is_null());
        assert_eq!(colors["caret"], "#2f6fb5");
    }

    #[test]
    fn weight_is_clamped() {
        // Given: weights below and above the range.
        // When / Then: they clamp to 100 and 900.
        assert_eq!(
            sanitize_weight(&serde_json::json!(20)),
            serde_json::json!(100)
        );
        assert_eq!(
            sanitize_weight(&serde_json::json!(1500)),
            serde_json::json!(900)
        );
    }

    #[test]
    fn broken_theme_does_not_hide_siblings() {
        // Given: a family where the second theme lacks `appearance`.
        let results = convert_family(&load(BROKEN));
        // When / Then: the first converts and the second reports BadAppearance.
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(ZedThemeError::BadAppearance { .. })
        ));
    }

    #[gpui_kit::test]
    fn converted_dark_theme_applies(cx: &mut TestAppContext) {
        // Given: Kit initialised and the converted dark config.
        let config = Rc::new(sample_configs().remove(1));
        cx.update(gpui_kit::init);
        // When: applying it to the global theme.
        cx.update(|cx| Theme::update(cx, |theme| theme.apply_config(&config)));
        // Then: the background matches and the theme is dark.
        cx.update(|cx| {
            let expected: Hsla = Rgba::try_from("#14161b").unwrap().into();
            assert_eq!(cx.theme().background, expected);
            assert!(cx.theme().is_dark());
        });
    }
}
