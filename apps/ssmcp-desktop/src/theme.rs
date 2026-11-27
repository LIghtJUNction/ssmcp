use zgpui::native::{Palette, Theme};

/// zgpui resolves its semantic tokens for every primitive and built-in widget.
pub fn palette(theme: Theme) -> Palette {
    let gray = |light, dark| {
        let value = if theme == Theme::Light { light } else { dark };
        u32::from_be_bytes([value, value, value, 255])
    };
    Palette {
        background: gray(250, 16),
        surface: gray(255, 24),
        text: gray(28, 239),
        muted: gray(94, 171),
        line: gray(210, 60),
        accent: gray(39, 229),
        on_accent: gray(255, 20),
        accent_hover: gray(60, 251),
        accent_soft: gray(233, 46),
        hover: gray(242, 38),
        success: gray(79, 184),
        success_soft: gray(237, 43),
        disabled_surface: gray(241, 34),
        disabled_text: gray(105, 145),
        // Retain the theme's error text and error surface.
        ..Palette::for_theme(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zgpui::native::colors;

    fn luminance(tone: u32) -> f64 {
        let channel = (tone >> 24) as f64 / 255.;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn widgets_resolve_to_neutral_colors_with_readable_text_in_both_themes() {
        for theme in [Theme::Dark, Theme::Light] {
            let p = palette(theme);
            for token in [
                colors::BG,
                colors::SURFACE,
                colors::TEXT,
                colors::MUTED,
                colors::LINE,
                colors::BLUE,
                colors::BLUE_PALE,
                colors::GREEN,
                0x1765ccff,
                0xf0f4faff,
                0xe6f4eaff,
                0xeff2f6ff,
                0x9aa5b5ff,
            ] {
                let [r, g, b, a] = p.color(token).to_be_bytes();
                assert_eq!((r, b, a), (g, g, 255));
            }
            for (ink, fill) in [
                (p.text, p.background),
                (p.muted, p.surface),
                (p.text, p.accent_soft),
                (p.on_accent, p.accent),
                (p.on_accent, p.accent_hover),
                (p.disabled_text, p.disabled_surface),
            ] {
                let (a, b) = (luminance(ink), luminance(fill));
                assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
            }
            assert_eq!(p.color(colors::RED), Palette::for_theme(theme).danger);
        }
    }
}
