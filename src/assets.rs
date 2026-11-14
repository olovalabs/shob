use gpui::{App, px, rgba};
use rust_embed::RustEmbed;

pub const SANS_FONT: &str = "IBM Plex Sans";
pub const MONO_FONT: &str = "Lilex";

#[derive(RustEmbed)]
#[folder = "assets/"]
// Unreferenced by the app (verified: no `file_icons/`, `logo/`, `ezicode`,
// or `olova` path is ever loaded); excluding them keeps ~3 MB out of the
// binary and out of every asset enumeration.
#[exclude = "file_icons/*"]
#[exclude = "logo/*"]
pub struct AppAssets;

pub struct CombinedAssets;

impl gpui::AssetSource for CombinedAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let clean = path
            .trim_start_matches("icons/")
            .trim_start_matches("assets/")
            .trim_start_matches('/');
        if clean == "logo/kathamo.png"
            || clean == "kathamo.png"
            || clean == "logo/markup.png"
            || clean == "markup.png"
        {
            return Ok(Some(std::borrow::Cow::Borrowed(crate::APP_ICON_PNG)));
        }
        if let Some(file) = AppAssets::get(clean) {
            return Ok(Some(file.data));
        }
        if let Some(file) = AppAssets::get(path) {
            return Ok(Some(file.data));
        }
        // Debug-only fallback to the working tree, so newly placed files work
        // without a rebuild. Release builds go straight to the component
        // assets instead of probing the filesystem on every miss.
        #[cfg(debug_assertions)]
        {
            let disk_paths = [
                std::path::Path::new("assets").join(path),
                std::path::Path::new("assets").join(clean),
                std::path::Path::new(clean).to_path_buf(),
                std::path::Path::new(path).to_path_buf(),
            ];
            for disk_path in disk_paths {
                if let Ok(bytes) = std::fs::read(&disk_path) {
                    return Ok(Some(std::borrow::Cow::Owned(bytes)));
                }
            }
        }
        if let Ok(Some(data)) = gpui::AssetSource::load(&gpui_kit::assets::Assets, path) {
            return Ok(Some(data));
        }
        gpui_component_assets::Assets::get(path)
            .map(|file| Some(file.data))
            .ok_or_else(|| anyhow::anyhow!("could not find asset at path \"{path}\""))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut list = Vec::new();
        for file in AppAssets::iter() {
            list.push(gpui::SharedString::from(file.to_string()));
        }
        list.extend(
            gpui_component_assets::Assets::iter()
                .filter(|asset| asset.starts_with(path))
                .map(|asset| asset.into()),
        );
        if let Ok(kit_assets) = gpui::AssetSource::list(&gpui_kit::assets::Assets, path) {
            list.extend(kit_assets);
        }
        Ok(list)
    }
}

pub fn load_embedded_fonts(cx: &App) {
    let fonts: Vec<std::borrow::Cow<'static, [u8]>> = AppAssets::iter()
        .filter(|p| p.starts_with("fonts/") && p.ends_with(".ttf"))
        .filter_map(|p| AppAssets::get(&p).map(|f| f.data))
        .collect();
    let _ = cx.text_system().add_fonts(fonts);
}

pub fn sync_component_fonts(cx: &mut App) {
    let theme = gpui_component::Theme::global_mut(cx);
    theme.font_family = SANS_FONT.into();
    theme.mono_font_family = MONO_FONT.into();
    theme.mono_font_size = px(13.5);
    theme.colors.muted_foreground = rgba(0x696969ff).into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource;

    #[test]
    fn loads_sidebar_icons() {
        let assets = CombinedAssets;
        assert!(assets.load("icons/panel-left.svg").unwrap().is_some());
        assert!(assets.load("icons/eye.svg").unwrap().is_some());
        assert!(assets.load("icons/command.svg").unwrap().is_some());
        assert!(assets.load("icons/plus.svg").unwrap().is_some());
        assert!(assets.load("window-close.svg").unwrap().is_some());
        assert!(assets.load("window-minimize.svg").unwrap().is_some());
        assert!(assets.load("window-maximize.svg").unwrap().is_some());
        assert!(assets.load("window-restore.svg").unwrap().is_some());
        assert!(assets.load("icons/split-vertical.svg").unwrap().is_some());
        assert!(assets.load("icons/split-horizontal.svg").unwrap().is_some());
        assert!(assets.load("icons/pane-maximize.svg").unwrap().is_some());
        assert!(assets.load("icons/pane-restore.svg").unwrap().is_some());
        assert!(assets.load("icons/pane-close.svg").unwrap().is_some());
        assert!(assets.load("icons/chevron-right.svg").unwrap().is_some());
        assert!(assets.load("icons/chevron-down.svg").unwrap().is_some());
        assert!(assets.load("icons/folder.svg").unwrap().is_some());
        assert!(assets.load("icons/git-branch.svg").unwrap().is_some());
        assert!(assets.load("icons/folder-plus.svg").unwrap().is_some());
        assert!(assets.load("icons/x.svg").unwrap().is_some());
        assert!(assets.load("icons/copy.svg").unwrap().is_some());
        assert!(assets.load("icons/clipboard.svg").unwrap().is_some());
        assert!(assets.load("icons/settings.svg").unwrap().is_some());
        assert!(assets.load("icons/arrow-left.svg").unwrap().is_some());
        assert!(assets.load("icons/sun.svg").unwrap().is_some());
        assert!(assets.load("icons/moon.svg").unwrap().is_some());
        assert!(assets.load("icons/monitor.svg").unwrap().is_some());
        assert!(assets.load("icons/ellipsis.svg").unwrap().is_some());
        assert!(assets.load("kathamo-wordmark.png").unwrap().is_some());
        assert!(assets.load("logo/kathamo.png").unwrap().is_some());
        let icon = image::load_from_memory(crate::APP_ICON_PNG)
            .unwrap()
            .into_rgba8();
        assert_eq!(icon.width(), 1254);
        assert_eq!(icon.height(), 1254);
    }
}
