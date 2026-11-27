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
#[exclude = "logos/shob.png"]
#[exclude = "icons/shob.png"]
pub struct AppAssets;

pub struct CombinedAssets;

impl gpui::AssetSource for CombinedAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let clean = path
            .trim_start_matches("icons/")
            .trim_start_matches("assets/")
            .trim_start_matches('/');
        if clean == "logo/shob.png"
            || clean == "logos/shob.png"
            || clean == "logos/shob.png"
            || clean == "shob.png"
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
        Err(anyhow::anyhow!("could not find asset at path \"{path}\""))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut list = Vec::new();
        for file in AppAssets::iter() {
            list.push(gpui::SharedString::from(file.to_string()));
        }
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
        assert!(assets.load("icons/window-close.svg").unwrap().is_some());
        assert!(assets.load("icons/window-minimize.svg").unwrap().is_some());
        assert!(assets.load("icons/window-maximize.svg").unwrap().is_some());
        assert!(assets.load("icons/window-restore.svg").unwrap().is_some());
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
        assert!(assets.load("icons/pencil.svg").unwrap().is_some());
        assert!(assets.load("icons/clipboard.svg").unwrap().is_some());
        assert!(assets.load("icons/settings.svg").unwrap().is_some());
        assert!(assets.load("icons/arrow-left.svg").unwrap().is_some());
        assert!(assets.load("icons/sun.svg").unwrap().is_some());
        assert!(assets.load("icons/moon.svg").unwrap().is_some());
        assert!(assets.load("icons/monitor.svg").unwrap().is_some());
        assert!(assets.load("icons/ellipsis.svg").unwrap().is_some());
        assert!(assets.load("icons/list-filter.svg").unwrap().is_some());
        assert!(assets.load("icons/square-pen.svg").unwrap().is_some());
        assert!(assets.load("icons/clock.svg").unwrap().is_some());
        assert!(assets.load("icons/archive.svg").unwrap().is_some());
        assert!(assets.load("icons/help.svg").unwrap().is_some());
        assert!(assets.load("shob-wordmark.png").unwrap().is_some());
        assert!(assets.load("logo/shob.png").unwrap().is_some());
        let icon = image::load_from_memory(crate::APP_ICON_PNG)
            .unwrap()
            .into_rgba8();
        assert_eq!(icon.width(), 1254);
        assert_eq!(icon.height(), 1254);
    }

    #[test]
    fn project_favicon_preserves_color_and_skips_invalid_candidates() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        std::fs::write(root.join("favicon.svg"), "<broken>").unwrap();
        std::fs::create_dir(root.join("public")).unwrap();
        std::fs::write(
            root.join("public/favicon.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"/>"#,
        )
        .unwrap();
        let public = root.join("apps/web/public/favicon");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(
            public.join("favicon.svg"),
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
                <rect width="8" height="16" fill="#863bff"
                    style="fill:#863bff;fill:color(display-p3 .5252 .23 1)"/>
                <rect x="8" width="8" height="16" fill="color(display-p3 0% 90% 80% / 75%)"/>
            </svg>"##,
        )
        .unwrap();

        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(()));
        let favicon = crate::load_project_favicon(root, &renderer).unwrap();
        let (pixels, _) = favicon.image.as_bytes(0).unwrap().as_chunks::<4>();
        assert!(pixels.contains(&[255, 53, 145, 255]));
        assert!(pixels.iter().any(|p| (203..=205).contains(&p[0])
            && (233..=235).contains(&p[1])
            && p[2] == 0
            && p[3] == 191));
        assert!(!favicon.needs_backdrop(true) && !favicon.needs_backdrop(false));

        let assets = root.join("assets");
        std::fs::create_dir(&assets).unwrap();
        image::RgbaImage::from_pixel(96, 48, image::Rgba([124, 58, 255, 255]))
            .save(assets.join("favicon.png"))
            .unwrap();
        let favicon = crate::load_project_favicon(root, &renderer).unwrap();
        assert_eq!(
            favicon.image.size(0),
            gpui::size(gpui::DevicePixels(64), gpui::DevicePixels(32))
        );
        assert_eq!(
            &favicon.image.as_bytes(0).unwrap()[..4],
            &[255, 58, 124, 255]
        );

        for (fill, dark_theme) in [("#222224", true), ("#f0f0f0", false)] {
            std::fs::write(
                root.join("favicon.svg"),
                format!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
                        <rect width="8" height="16" fill="{fill}"/>
                        <rect x="8" width="1" height="1" fill="orange"/>
                    </svg>"#
                ),
            )
            .unwrap();
            let favicon = crate::load_project_favicon(root, &renderer).unwrap();
            assert!(favicon.needs_backdrop(dark_theme));
            assert!(!favicon.needs_backdrop(!dark_theme));
        }
    }
}
