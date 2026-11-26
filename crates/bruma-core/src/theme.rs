//! The visual language: mist outside the selection, frosted glass for every
//! panel, one accent, six annotation inks.

/// Panels.
pub const INK: [u8; 3] = [0x0E, 0x0E, 0x0E];
/// Lines and text.
pub const PAPER: [u8; 3] = [0xF2, 0xF0, 0xEB];
/// The single accent.
pub const ACCENT: [u8; 3] = [0xFF, 0x4F, 0x1F];
/// Secondary text.
pub const MUTED: [u8; 3] = [0x88, 0x87, 0x80];
/// Panel outlines and separators.
pub const BORDER: [u8; 3] = [0x3A, 0x3A, 0x38];

/// Annotation inks, selected with the keys `1`–`6`.
pub const PALETTE: [[u8; 3]; 6] = [
    [0xE5, 0x34, 0x2B],
    [0xFF, 0xD6, 0x0A],
    [0x2F, 0xBF, 0x71],
    [0x2F, 0x7B, 0xF6],
    [0xFF, 0xFF, 0xFF],
    [0x0E, 0x0E, 0x0E],
];

/// Hints and other quiet text on glass.
pub const FAINT: [u8; 3] = [0xB0, 0xAE, 0xA6];

/// Brightness kept by the frozen screen before anything is selected. It stays
/// sharp so you can see what you are choosing.
pub const DIM_KEEP: f32 = 0.72;
/// Brightness kept by the mist: everything outside the selection.
pub const SOFT_KEEP: f32 = 0.6;
/// How dark the tint over frosted glass is.
pub const GLASS_TINT: f32 = 0.58;

pub const STROKE_MIN: f32 = 1.0;
pub const STROKE_MAX: f32 = 24.0;

/// Ink colour for a palette index; out-of-range indices wrap.
pub fn ink(index: u8) -> [u8; 3] {
    PALETTE[index as usize % PALETTE.len()]
}

/// Black or white, whichever reads better on `color`.
pub fn contrast(color: [u8; 3]) -> [u8; 3] {
    let luma = 0.299 * color[0] as f32 + 0.587 * color[1] as f32 + 0.114 * color[2] as f32;
    if luma > 150.0 { INK } else { [255, 255, 255] }
}

pub fn hex(color: [u8; 4]) -> String {
    format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2])
}

/// `rgb(255, 79, 31)`, as CSS writes it.
pub fn rgb(color: [u8; 4]) -> String {
    format!("rgb({}, {}, {})", color[0], color[1], color[2])
}

/// `hsl(13, 100%, 56%)`, as CSS writes it.
pub fn hsl(color: [u8; 4]) -> String {
    let [r, g, b] = [color[0], color[1], color[2]].map(|v| v as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let light = (max + min) / 2.0;
    let spread = max - min;
    let (hue, saturation) = if spread == 0.0 {
        (0.0, 0.0)
    } else {
        let saturation = spread / (1.0 - (2.0 * light - 1.0).abs());
        let hue = if max == r {
            ((g - b) / spread).rem_euclid(6.0)
        } else if max == g {
            (b - r) / spread + 2.0
        } else {
            (r - g) / spread + 4.0
        };
        (hue * 60.0, saturation)
    };
    format!(
        "hsl({:.0}, {:.0}%, {:.0}%)",
        hue,
        saturation * 100.0,
        light * 100.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_colour_formats() {
        assert_eq!(rgb([255, 79, 31, 255]), "rgb(255, 79, 31)");
        assert_eq!(hsl([255, 0, 0, 255]), "hsl(0, 100%, 50%)");
        assert_eq!(hsl([0, 0, 255, 255]), "hsl(240, 100%, 50%)");
        assert_eq!(hsl([128, 128, 128, 255]), "hsl(0, 0%, 50%)");
        assert_eq!(hsl([255, 255, 255, 255]), "hsl(0, 0%, 100%)");
    }

    #[test]
    fn hex_is_uppercase_rgb() {
        assert_eq!(hex([255, 79, 31, 255]), "#FF4F1F");
    }

    #[test]
    fn contrast_picks_readable_colour() {
        assert_eq!(contrast(PALETTE[1]), INK);
        assert_eq!(contrast(PALETTE[3]), [255, 255, 255]);
    }
}
