use gpui::Hsla;
use gpui_component::CaptionButtonHighlight;
use oito2_settings::foundation::DesktopEnvironment;

use super::{Icons, plasma_breeze};

type Highlights = [Option<CaptionButtonHighlight>; 4];

pub(super) struct Style {
    pub desktop: DesktopEnvironment,
    pub icon_theme_prefix: &'static str,
    pub highlights: fn(inverse: &Icons, light: &Icons, foreground: Hsla) -> Highlights,
}

const STYLES: &[Style] = &[plasma_breeze::STYLE];

pub(super) fn resolve(
    desktop: Option<DesktopEnvironment>,
    icon_theme: &str,
) -> Option<&'static Style> {
    let desktop = desktop?;
    STYLES.iter().find(|style| {
        style.desktop == desktop
            && icon_theme
                .get(..style.icon_theme_prefix.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(style.icon_theme_prefix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plasma_breeze_matches_desktop_and_icon_theme() {
        for (desktop, theme, matches) in [
            (DesktopEnvironment::Kde, "Breeze", true),
            (DesktopEnvironment::Kde, "breeze-dark", true),
            (DesktopEnvironment::Kde, "Adwaita", false),
            (DesktopEnvironment::Gnome, "Breeze", false),
        ] {
            assert_eq!(resolve(Some(desktop), theme).is_some(), matches);
        }
    }
}
