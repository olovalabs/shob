use gpui::{px, rgb};
use gpui_component::CaptionButtonHighlight;
use oito2_settings::foundation::DesktopEnvironment;

use super::styles::Style;

pub(super) const STYLE: Style = Style {
    desktop: DesktopEnvironment::Kde,
    icon_theme_prefix: "breeze",
    highlights: |inverse, light, foreground| {
        let glyph_highlight = |index: usize| {
            Some(CaptionButtonHighlight {
                glyph: inverse[index].clone()?,
                diameter: px(18.),
                hovered: foreground,
                pressed: foreground.opacity(0.5),
            })
        };
        [
            glyph_highlight(0),
            glyph_highlight(1),
            glyph_highlight(2),
            light[3].clone().map(|glyph| CaptionButtonHighlight {
                glyph,
                diameter: px(18.),
                hovered: rgb(0xff98a2).into(),
                pressed: rgb(0x6d2229).into(),
            }),
        ]
    },
};
