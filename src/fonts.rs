//! Fonts, embedded in the binary (all SIL OFL, see assets/fonts/*-OFL.txt):
//!  * Inter          - UI text (Regular + SemiBold for titles). Covers Latin, Latin Extended-A
//!    and Cyrillic itself, so no fallback font is needed. The embedded files are subsets
//!    (only the code points listed below + kerning), converted to TrueType outlines.
//!  * JetBrains Mono - monospaced values (pids, geometry, ...)
//!
//! Sizes are chosen to match ImGui's built-in 13 px font in visual size: Inter has a large
//! x-height, so 14 px looks about as big as ProggyClean at 13 px. Titles use the SAME size as
//! the body text and are set apart by weight and colour only, so they never look oversized.
//!
//! Must run BEFORE the Vulkan renderer is created, because the renderer uploads the font atlas.

use imgui::{Context, FontConfig, FontGlyphRanges, FontId, FontSource};

static SANS_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
static SANS_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
static MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

const SANS_SIZE: f32 = 14.0;
const MONO_SIZE: f32 = 13.0;

/// Basic Latin, Latin-1, Latin Extended-A (Czech, Polish, ...), Cyrillic, general punctuation,
/// euro, arrows, minus. Zero-terminated pairs of (first, last) code points.
const GLYPHS: &[u32] = &[
    0x0020, 0x017F, 0x0192, 0x0192, 0x0400, 0x052F, 0x2010, 0x2027, 0x20A0, 0x20BF, 0x2190,
    0x2193, 0x2212, 0x2212, 0,
];

#[derive(Clone, Copy)]
pub struct Fonts {
    pub regular: FontId,
    pub bold: FontId,
    pub mono: FontId,
}

fn config(ranges: &'static [u32]) -> FontConfig {
    // Small text: NO oversampling + whole-pixel glyph advances keep stems on the pixel
    // grid. Oversampling (3x2) plus bilinear atlas sampling made 13-14 px text blurry.
    let cfg = FontConfig {
        oversample_h: 1,
        oversample_v: 1,
        pixel_snap_h: true,
        glyph_ranges: FontGlyphRanges::from_slice(ranges),
        // stb_truetype can't hint, so thin strokes come out faint/soft; darken the edges a bit
        rasterizer_multiply: 1.25,
        ..FontConfig::default()
    };
    // With the optional FreeType builder (`cargo build --release --features freetype`) the
    // glyphs get real auto-hinting instead: vertical snapping to the pixel grid = crisp text.
    #[cfg(feature = "freetype")]
    let cfg = FontConfig {
        // ForceAutoHint (1<<2) | LightHinting (1<<3); our subset fonts carry no native hints
        font_builder_flags: (1 << 2) | (1 << 3),
        rasterizer_multiply: 1.0,
        ..cfg
    };
    cfg
}

/// Loads all fonts; the first one (regular) becomes ImGui's default font.
pub fn load(ctx: &mut Context) -> Fonts {
    let fonts = ctx.fonts();
    let regular = fonts.add_font(&[FontSource::TtfData {
        data: SANS_REGULAR,
        size_pixels: SANS_SIZE,
        config: Some(config(GLYPHS)),
    }]);
    // `bold` is the title font (SemiBold is enough contrast and looks less heavy than Bold)
    let bold = fonts.add_font(&[FontSource::TtfData {
        data: SANS_SEMIBOLD,
        size_pixels: SANS_SIZE,
        config: Some(config(GLYPHS)),
    }]);
    let mono = fonts.add_font(&[FontSource::TtfData {
        data: MONO_REGULAR,
        size_pixels: MONO_SIZE,
        config: Some(config(GLYPHS)),
    }]);
    Fonts {
        regular,
        bold,
        mono,
    }
}
