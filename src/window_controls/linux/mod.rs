mod plasma_breeze;
mod styles;

use gpui::{App, Context, Global, RenderImage, Window};
use gpui_component::{ActiveTheme as _, CaptionButton, WindowControls};
use oito2_settings::{
    SystemSettings,
    foundation::{DesktopEnvironment, WindowButton},
};
use std::{io::Read, path::Path, process::Stdio, sync::Arc, time::Duration};

type Icons = [Option<Arc<RenderImage>>; 4];

struct Controls {
    settings: SystemSettings,
    light: Icons,
    dark: Icons,
    style: Option<&'static styles::Style>,
}

impl Global for Controls {}

pub(crate) fn controls(cx: &App, dark: bool, leading: bool) -> Option<WindowControls> {
    let controls = cx.try_global::<Controls>()?;
    let layout = &controls.settings.window_button_layout;
    let buttons = if leading {
        &layout.leading
    } else {
        &layout.trailing
    };
    let buttons = std::array::from_fn(|index| {
        Some(match buttons.get(index)? {
            WindowButton::Minimize => CaptionButton::Minimize,
            WindowButton::Maximize => CaptionButton::Maximize,
            WindowButton::Close => CaptionButton::Close,
        })
    });
    let icons = if dark {
        &controls.dark
    } else {
        &controls.light
    };
    let highlights = controls.style.map_or([None, None, None, None], |style| {
        let inverse = if dark {
            &controls.light
        } else {
            &controls.dark
        };
        (style.highlights)(inverse, &controls.light, cx.theme().foreground)
    });
    Some(WindowControls::new().linux_theme(buttons, icons.clone(), highlights))
}

pub(crate) fn load<T: 'static>(window: &mut Window, cx: &mut Context<T>) {
    let (changes, changed) = smol::channel::bounded(1);
    let activation = changes.clone();
    cx.observe_window_activation(window, move |_, window, _| {
        if window.is_window_active() {
            let _ = activation.try_send(());
        }
    })
    .detach();

    let (updates, updated) = smol::channel::bounded(1);
    let worker = cx.background_executor().spawn(async move {
        let _ = changes.try_send(());
        let _watch = oito2_settings::watch(move |_| {
            let _ = changes.try_send(());
        })
        .ok();
        let mut previous = None;
        while changed.recv().await.is_ok() {
            let settings = oito2_settings::read_system_settings();
            let theme = icon_theme(settings.desktop_environment).await;
            if previous.as_ref().is_some_and(|(old_settings, old_theme)| {
                old_settings == &settings && old_theme == &theme
            }) {
                continue;
            }
            let icons = [
                "window-minimize-symbolic",
                "window-maximize-symbolic",
                "window-restore-symbolic",
                "window-close-symbolic",
            ]
            .map(|name| {
                let path = freedesktop_icons::lookup(name)
                    .with_size(16)
                    .force_svg()
                    .with_theme(&theme)
                    .find()?;
                read_icon(&path)
            });
            let controls = Controls {
                settings: settings.clone(),
                style: styles::resolve(settings.desktop_environment, &theme),
                light: icons.each_ref().map(|icon| tint(icon.as_ref(), 32)),
                dark: icons.each_ref().map(|icon| tint(icon.as_ref(), 240)),
            };
            previous = Some((settings, theme));
            if updates.send(controls).await.is_err() {
                break;
            }
        }
    });
    cx.spawn(async move |view, cx| {
        let _worker = worker;
        while let Ok(controls) = updated.recv().await {
            if view
                .update(cx, |_, cx| {
                    cx.set_global(controls);
                    cx.refresh_windows();
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

async fn query(program: &str, args: &[&str]) -> Option<String> {
    smol::future::race(
        async {
            let result = smol::process::Command::new(program)
                .args(args)
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .ok()?;
            let value = String::from_utf8(result.stdout).ok()?;
            let value = value.trim().trim_matches('\'');
            (result.status.success() && !value.is_empty()).then(|| value.to_owned())
        },
        async {
            smol::Timer::after(Duration::from_secs(1)).await;
            None
        },
    )
    .await
}

async fn icon_theme(desktop: Option<DesktopEnvironment>) -> String {
    let theme = match desktop {
        Some(DesktopEnvironment::Kde) => {
            let args = [
                "--file",
                "kdeglobals",
                "--group",
                "Icons",
                "--key",
                "Theme",
                "--default",
                "breeze",
            ];
            match query("kreadconfig6", &args).await {
                Some(theme) => Some(theme),
                None => query("kreadconfig5", &args).await,
            }
        }
        Some(DesktopEnvironment::Xfce) => {
            query(
                "xfconf-query",
                &["-c", "xsettings", "-p", "/Net/IconThemeName"],
            )
            .await
        }
        Some(DesktopEnvironment::Cinnamon) => {
            query(
                "gsettings",
                &["get", "org.cinnamon.desktop.interface", "icon-theme"],
            )
            .await
        }
        Some(DesktopEnvironment::Mate) => {
            query("gsettings", &["get", "org.mate.interface", "icon-theme"]).await
        }
        _ => None,
    };
    if let Some(theme) = theme {
        return theme;
    }
    query(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "icon-theme"],
    )
    .await
    .unwrap_or_else(|| "hicolor".to_owned())
}

fn read_icon(path: &Path) -> Option<resvg::tiny_skia::Pixmap> {
    use resvg::{tiny_skia, usvg};
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 128 * 1024 {
        return None;
    }
    let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).ok()?;
    let mut pixels = tiny_skia::Pixmap::new(32, 32)?;
    let scale = (32.0 / tree.size().width()).min(32.0 / tree.size().height());
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(
        (32.0 - tree.size().width() * scale) / 2.0,
        (32.0 - tree.size().height() * scale) / 2.0,
    );
    resvg::render(&tree, transform, &mut pixels.as_mut());
    pixels
        .pixels()
        .iter()
        .any(|pixel| pixel.alpha() != 0)
        .then_some(pixels)
}

fn tint(icon: Option<&resvg::tiny_skia::Pixmap>, foreground: u8) -> Option<Arc<RenderImage>> {
    let icon = icon?;
    let frame = image::RgbaImage::from_fn(32, 32, |x, y| {
        let alpha = icon.pixels()[(y * 32 + x) as usize].alpha();
        image::Rgba([foreground, foreground, foreground, alpha])
    });
    Some(Arc::new(RenderImage::new([image::Frame::new(frame)])))
}
