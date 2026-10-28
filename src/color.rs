//! Color math: generated connection colors, contrast and Peacock derivation.
//!
//! Connection colors are generated in OKLCH from a pinned FNV-1a hash of the
//! connection seed, so they stay stable across runs, platforms and releases.

use gpui_kit::{Hsla, Rgba};

/// Errors raised while parsing a color.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ColorError {
    /// The input is not a `#RRGGBB` / `RRGGBB` hex color.
    #[error("invalid hex color: {0:?}")]
    InvalidHex(String),
}

/// An opaque sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

/// Rounds a channel value to `u8`, clamping to `0..=255` first.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=255 before the cast"
)]
fn round_channel(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

impl Rgb {
    /// Formats the color as uppercase `#RRGGBB`.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Parses `#RRGGBB` or `RRGGBB`, case-insensitive.
    ///
    /// # Errors
    /// Returns [`ColorError::InvalidHex`] for any other input.
    pub fn parse_hex(input: &str) -> Result<Self, ColorError> {
        let invalid = || ColorError::InvalidHex(input.to_owned());
        let digits = input.strip_prefix('#').unwrap_or(input);
        if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let channel = |range: std::ops::Range<usize>| {
            digits
                .get(range)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(invalid)
        };
        Ok(Self {
            r: channel(0..2)?,
            g: channel(2..4)?,
            b: channel(4..6)?,
        })
    }

    /// Converts to GPUI's [`Hsla`].
    #[must_use]
    pub fn to_hsla(self) -> Hsla {
        Rgba {
            r: f32::from(self.r) / 255.0,
            g: f32::from(self.g) / 255.0,
            b: f32::from(self.b) / 255.0,
            a: 1.0,
        }
        .into()
    }

    /// Converts from GPUI's [`Hsla`], ignoring alpha.
    #[must_use]
    pub fn from_hsla(color: Hsla) -> Self {
        let rgba = Rgba::from(color);
        Self {
            r: round_channel(f64::from(rgba.r) * 255.0),
            g: round_channel(f64::from(rgba.g) * 255.0),
            b: round_channel(f64::from(rgba.b) * 255.0),
        }
    }
}

/// 64-bit FNV-1a hash (pinned so persisted colors never change).
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Hue in degrees (`0.0..360.0`, 0.1 resolution) derived from a seed.
#[must_use]
pub fn seed_hue(seed: &str) -> f64 {
    let tenths = u16::try_from(fnv1a_64(seed.as_bytes()) % 3600).unwrap_or_default();
    f64::from(tenths) / 10.0
}

/// Converts `OKLab` to linear sRGB (Björn Ottosson's matrices).
#[must_use]
pub fn oklab_to_linear_srgb(l: f64, a: f64, b: f64) -> [f64; 3] {
    let l_ = (0.396_337_777_4_f64.mul_add(a, l) + 0.215_803_757_3 * b).powi(3);
    let m_ = (l - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
    let s_ = (l - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
    [
        4.076_741_662_1 * l_ - 3.307_711_591_3 * m_ + 0.230_969_929_2 * s_,
        -1.268_438_004_6 * l_ + 2.609_757_401_1 * m_ - 0.341_319_396_5 * s_,
        -0.004_196_086_3 * l_ - 0.703_418_614_7 * m_ + 1.707_614_701_0 * s_,
    ]
}

fn gamma_encode(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Converts `OKLCH` to sRGB, reducing chroma until the color is in gamut.
#[must_use]
pub fn oklch_to_rgb(lightness: f64, chroma: f64, hue_deg: f64) -> Rgb {
    let hue = hue_deg.to_radians();
    let mut reduced = chroma;
    let linear = loop {
        let candidate = oklab_to_linear_srgb(lightness, reduced * hue.cos(), reduced * hue.sin());
        let in_gamut = candidate.iter().all(|v| (0.0..=1.0).contains(v));
        if in_gamut || reduced <= 0.0 {
            break candidate;
        }
        reduced = (reduced - 0.005).max(0.0);
    };
    let [red, green, blue] = linear.map(|v| round_channel(gamma_encode(v.clamp(0.0, 1.0)) * 255.0));
    Rgb {
        r: red,
        g: green,
        b: blue,
    }
}

/// `OKLCH` lightness of generated connection colors.
///
/// 0.68 keeps every hue at WCAG AA contrast (>= 4.5) with the Peacock
/// foreground chosen by [`readable_foreground`].
pub const GENERATED_LIGHTNESS: f64 = 0.68;
/// `OKLCH` chroma of generated connection colors.
pub const GENERATED_CHROMA: f64 = 0.14;

/// Deterministic connection color for a seed.
#[must_use]
pub fn generated_color(seed: &str) -> Rgb {
    oklch_to_rgb(GENERATED_LIGHTNESS, GENERATED_CHROMA, seed_hue(seed))
}

/// Twelve preset colors, hues 0°, 30°, ... 330°.
#[must_use]
pub fn connection_presets() -> [Rgb; 12] {
    std::array::from_fn(|index| {
        let hue = 30.0 * f64::from(u8::try_from(index).unwrap_or_default());
        oklch_to_rgb(GENERATED_LIGHTNESS, GENERATED_CHROMA, hue)
    })
}

/// Perceived brightness (YIQ), `0.0..=255.0`.
#[must_use]
pub fn yiq_brightness(color: Rgb) -> f64 {
    (299.0 * f64::from(color.r) + 587.0 * f64::from(color.g) + 114.0 * f64::from(color.b)) / 1000.0
}

/// Whether the color is light (tinycolor semantics: brightness >= 128).
#[must_use]
pub fn is_light(color: Rgb) -> bool {
    yiq_brightness(color) >= 128.0
}

/// WCAG relative luminance.
#[must_use]
pub fn relative_luminance(color: Rgb) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// WCAG contrast ratio between two colors (`1.0..=21.0`).
#[must_use]
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Peacock's dark foreground, `#15202B`.
pub const PEACOCK_DARK_FOREGROUND: Rgb = Rgb {
    r: 0x15,
    g: 0x20,
    b: 0x2B,
};
/// Peacock's light foreground, `#E7E7E7`.
pub const PEACOCK_LIGHT_FOREGROUND: Rgb = Rgb {
    r: 0xE7,
    g: 0xE7,
    b: 0xE7,
};
/// Peacock's inactive element alpha (`0x99 / 0xff`).
pub const INACTIVE_ALPHA: f32 = 153.0 / 255.0;

/// Picks whichever Peacock foreground has the higher WCAG contrast with `bg`.
///
/// Deliberately better than Peacock's YIQ rule, which can pick a
/// low-contrast foreground for mid-tone backgrounds.
#[must_use]
pub fn readable_foreground(bg: Rgb) -> Rgb {
    if contrast_ratio(bg, PEACOCK_DARK_FOREGROUND) >= contrast_ratio(bg, PEACOCK_LIGHT_FOREGROUND) {
        PEACOCK_DARK_FOREGROUND
    } else {
        PEACOCK_LIGHT_FOREGROUND
    }
}

struct Hsl {
    hue: f64,
    saturation: f64,
    lightness: f64,
}

fn to_hsl(color: Rgb) -> Hsl {
    let red = f64::from(color.r) / 255.0;
    let green = f64::from(color.g) / 255.0;
    let blue = f64::from(color.b) / 255.0;
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = f64::midpoint(max, min);
    let delta = max - min;
    if delta == 0.0 {
        return Hsl {
            hue: 0.0,
            saturation: 0.0,
            lightness,
        };
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let sector = if max.to_bits() == red.to_bits() {
        (green - blue) / delta + if green < blue { 6.0 } else { 0.0 }
    } else if max.to_bits() == green.to_bits() {
        (blue - red) / delta + 2.0
    } else {
        (red - green) / delta + 4.0
    };
    Hsl {
        hue: sector / 6.0,
        saturation,
        lightness,
    }
}

fn from_hsl(color: &Hsl) -> Rgb {
    let Hsl {
        hue,
        saturation,
        lightness,
    } = *color;
    if saturation == 0.0 {
        let gray = round_channel(lightness * 255.0);
        return Rgb {
            r: gray,
            g: gray,
            b: gray,
        };
    }
    let upper = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let lower = 2.0 * lightness - upper;
    let channel = |offset: f64| {
        let t = (hue + offset).rem_euclid(1.0);
        let value = if t < 1.0 / 6.0 {
            (upper - lower).mul_add(6.0 * t, lower)
        } else if t < 0.5 {
            upper
        } else if t < 2.0 / 3.0 {
            (upper - lower).mul_add((2.0 / 3.0 - t) * 6.0, lower)
        } else {
            lower
        };
        round_channel(value * 255.0)
    };
    Rgb {
        r: channel(1.0 / 3.0),
        g: channel(0.0),
        b: channel(-1.0 / 3.0),
    }
}

/// Raises HSL lightness by `percent` points (tinycolor `lighten`).
#[must_use]
pub fn lighten(color: Rgb, percent: f64) -> Rgb {
    let hsl = to_hsl(color);
    from_hsl(&Hsl {
        lightness: (hsl.lightness + percent / 100.0).clamp(0.0, 1.0),
        ..hsl
    })
}

/// Lowers HSL lightness by `percent` points (tinycolor `darken`).
#[must_use]
pub fn darken(color: Rgb, percent: f64) -> Rgb {
    lighten(color, -percent)
}

/// Hover shade: darker on light backgrounds, lighter on dark ones.
#[must_use]
pub fn hover_variant(bg: Rgb) -> Rgb {
    if is_light(bg) {
        darken(bg, 10.0)
    } else {
        lighten(bg, 10.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn fnv1a_64_matches_reference_vectors() {
        // Given the published FNV-1a 64-bit test vectors
        // When hashing them
        // Then the digests match
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn hex_round_trips_uppercase() {
        // Given a color
        let color = rgb(0x12, 0xab, 0xEF);
        // When formatting then parsing with and without '#', in any case
        // Then the original comes back
        assert_eq!(color.to_hex(), "#12ABEF");
        assert_eq!(Rgb::parse_hex("#12ABEF").unwrap(), color);
        assert_eq!(Rgb::parse_hex("12abef").unwrap(), color);
    }

    #[test]
    fn parse_hex_rejects_invalid() {
        // Given malformed inputs
        // When parsing
        // Then each is an error
        for input in ["#12345", "zzzzzz", "#1234567", "", "#", "+12345", "#12é456"] {
            assert!(Rgb::parse_hex(input).is_err(), "{input:?} must be rejected");
        }
    }

    #[test]
    fn oklab_primaries_convert() {
        // Given the OKLab coordinates of sRGB red
        // When converting to linear sRGB
        let [r, g, b] = oklab_to_linear_srgb(0.627_955, 0.224_863, 0.125_846);
        // Then they are approximately [1, 0, 0]
        assert!((r - 1.0).abs() < 0.001 && g.abs() < 0.001 && b.abs() < 0.001);
        assert_eq!(oklch_to_rgb(1.0, 0.0, 0.0), rgb(255, 255, 255));
        assert_eq!(oklch_to_rgb(0.0, 0.0, 0.0), rgb(0, 0, 0));
    }

    #[test]
    fn generated_color_is_deterministic_and_pinned() {
        // Given a seed
        let seed = "postgres://localhost:5432";
        // When generating twice
        // Then results agree and match the pinned golden value
        assert_eq!(generated_color(seed), generated_color(seed));
        assert_eq!(generated_color(seed).to_hex(), "#CF74B5");
    }

    #[test]
    fn generated_colors_spread_over_hues() {
        // Given 360 distinct seeds
        let mut buckets = [false; 12];
        for i in 0..360 {
            let hue = seed_hue(&format!("postgres://host{i}:5432"));
            // When bucketing their hues by 30 degrees
            for (bucket, index) in buckets.iter_mut().zip(0u8..) {
                let start = 30.0 * f64::from(index);
                if hue >= start && hue < start + 30.0 {
                    *bucket = true;
                }
            }
        }
        // Then at least 11 buckets are used
        assert!(buckets.iter().filter(|b| **b).count() >= 11);
    }

    #[test]
    fn generated_colors_are_legible_for_every_hue() {
        // Given the generated lightness/chroma at every integer hue
        for hue in 0..360u16 {
            let color = oklch_to_rgb(GENERATED_LIGHTNESS, GENERATED_CHROMA, f64::from(hue));
            // When picking the readable foreground
            let ratio = contrast_ratio(color, readable_foreground(color));
            // Then contrast reaches WCAG AA
            assert!(ratio >= 4.5, "hue {hue}: {ratio}");
        }
    }

    #[test]
    fn presets_are_twelve_distinct_colors() {
        // Given the presets
        let presets = connection_presets();
        // When deduplicating
        let unique: std::collections::HashSet<_> = presets.iter().copied().collect();
        // Then all twelve differ
        assert_eq!(unique.len(), 12);
    }

    #[test]
    fn custom_colors_get_a_legible_foreground() {
        // Given a 16-step grid per channel
        for r in 0..16u8 {
            for g in 0..16u8 {
                for b in 0..16u8 {
                    let bg = rgb(r * 17, g * 17, b * 17);
                    // When choosing the foreground
                    let chosen = readable_foreground(bg);
                    let other = if chosen == PEACOCK_DARK_FOREGROUND {
                        PEACOCK_LIGHT_FOREGROUND
                    } else {
                        PEACOCK_DARK_FOREGROUND
                    };
                    // Then it is at least 3.0 and never worse than the alternative
                    let ratio = contrast_ratio(bg, chosen);
                    assert!(ratio >= 3.0, "{bg:?}: {ratio}");
                    assert!(ratio >= contrast_ratio(bg, other));
                }
            }
        }
    }

    #[test]
    fn lighten_and_darken_follow_tinycolor() {
        // Given white and black
        // When darkening / lightening by 10 percent
        // Then the results match tinycolor
        assert_eq!(darken(rgb(255, 255, 255), 10.0).to_hex(), "#E6E6E6");
        assert_eq!(lighten(rgb(0, 0, 0), 10.0).to_hex(), "#1A1A1A");
    }

    #[test]
    fn hover_variant_direction_follows_is_light() {
        // Given a light and a dark color
        let light = rgb(250, 250, 250);
        let dark = rgb(10, 10, 10);
        // When deriving hover variants
        // Then light darkens and dark lightens
        assert!(is_light(light));
        assert!(!is_light(dark));
        assert!(yiq_brightness(hover_variant(light)) < yiq_brightness(light));
        assert!(yiq_brightness(hover_variant(dark)) > yiq_brightness(dark));
    }

    #[test]
    fn hsla_conversion_round_trips() {
        // Given a color
        let color = rgb(200, 100, 50);
        // When converting to Hsla and back
        // Then it is unchanged
        assert_eq!(Rgb::from_hsla(color.to_hsla()), color);
    }

    #[test]
    fn contrast_ratio_extremes() {
        // Given black on white
        let ratio = contrast_ratio(rgb(0, 0, 0), rgb(255, 255, 255));
        // Then the ratio is 21
        assert!((ratio - 21.0).abs() < 1e-9);
    }
}
