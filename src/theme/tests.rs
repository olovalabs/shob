use super::*;

#[test]
fn gradient_ascii_maps_the_known_cli_panel_background() {
    let palette = ThemeColors::complete_defaults(PaletteScheme::MidnightGarden)
        .palette(PaletteScheme::MidnightGarden, true);
    let colors = alacritty_terminal::term::color::Colors::default();
    let source = alacritty_terminal::vte::ansi::Color::Spec(alacritty_terminal::vte::ansi::Rgb {
        r: 15,
        g: 18,
        b: 22,
    });
    let mapped = palette.map_background(palette.resolve(source, &colors));
    assert_eq!(
        terminal::hsla_to_rgb(mapped),
        alacritty_terminal::vte::ansi::Rgb {
            r: 10,
            g: 33,
            b: 71
        }
    );
}

#[test]
fn midnight_garden_switches_default_colors_and_round_trips() {
    let mut colors = ThemeColors::complete_defaults(PaletteScheme::Warp);
    colors.change_scheme(PaletteScheme::Warp, PaletteScheme::MidnightGarden);
    let tokens = colors.tokens(
        gpui_component::ThemeMode::Dark,
        PaletteScheme::MidnightGarden,
    );
    assert_eq!(tokens.pane_bg, rgba(0x001536ff).into());
    assert_eq!(tokens.model_picker_bg, tokens.card_bg);
    assert_eq!(tokens.model_picker_accent, rgba(0xe38a94ff).into());
    assert_eq!(
        colors
            .palette(PaletteScheme::MidnightGarden, true)
            .background(),
        load_palette(PaletteScheme::MidnightGarden, true).background()
    );
    assert_eq!(
        serde_json::to_string(&PaletteScheme::MidnightGarden).unwrap(),
        "\"midnight_garden\""
    );
    colors.change_scheme(PaletteScheme::MidnightGarden, PaletteScheme::Warp);
    assert_eq!(
        colors.dark.style,
        ThemeColors::complete_defaults(PaletteScheme::Warp)
            .dark
            .style
    );
}

#[test]
fn generated_defaults_match_built_in_and_keep_customizations_on_scheme_change() {
    let mut colors = ThemeColors::complete_defaults(PaletteScheme::Warp);
    let actual = colors.tokens(gpui_component::ThemeMode::Dark, PaletteScheme::Warp);
    let original =
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Dark, PaletteScheme::Warp);
    assert_eq!(actual.window_bg, original.window_bg);
    assert_eq!(actual.model_picker_bg, original.card_bg);
    assert_eq!(actual.model_picker_accent, original.model_picker_accent);
    assert_eq!(
        colors.palette(PaletteScheme::Warp, true).ansi_colors(),
        load_warp_palette(true).ansi_colors()
    );
    colors
        .dark
        .style
        .insert("window.bg".into(), "#123456".into());
    colors
        .dark
        .style
        .insert("model_picker.bg".into(), "#181818ff".into());
    colors
        .dark
        .style
        .insert("model_picker.accent".into(), "#456789".into());
    colors.remove_generated_picker_colors(PaletteScheme::Warp);
    assert!(!colors.dark.style.contains_key("model_picker.bg"));
    colors.change_scheme(PaletteScheme::Warp, PaletteScheme::Github);
    assert_eq!(colors.dark.style["window.bg"], "#123456");
    assert_eq!(
        colors
            .tokens(gpui_component::ThemeMode::Dark, PaletteScheme::Github)
            .model_picker_accent,
        rgba(0x456789ff).into()
    );
    assert_eq!(
        colors.dark.style["sidebar.bg"],
        ThemeColors::complete_defaults(PaletteScheme::Github)
            .dark
            .style["sidebar.bg"]
    );
}

#[test]
fn overrides_apply_per_mode_and_preserve_palette_defaults() {
    let colors: ThemeColors = serde_json::from_str(
        r##"{
            "dark": {"ui": {"window_bg": "#12345680", "pane.bg": "#182838", "text.primary": "#abcdef", "editor.bar.bg": "#234567", "primary.btn.bg": "#456789", "chat.user.bg": "#1c1c1c", "chat.user.fg": "#fafafa", "chat.user.border": "#333333", "card.bg": "#345678", "ring": "#6789ab"},
                     "terminal": {"red": "#abcdef"}}
        }"##,
    )
    .unwrap();
    let dark = colors.tokens(gpui_component::ThemeMode::Dark, PaletteScheme::Warp);
    assert_eq!(dark.window_bg, rgba(0x12345680).into());
    assert_eq!(dark.primary_btn_bg, rgba(0x456789ff).into());
    assert_eq!(dark.chat().background, rgba(0x182838ff).into());
    assert_eq!(dark.chat().foreground, rgba(0xabcdefff).into());
    assert_eq!(dark.chat().composer, rgba(0x234567ff).into());
    assert_eq!(dark.chat().user_background, rgba(0x1c1c1cff).into());
    assert_eq!(dark.chat().user_foreground, rgba(0xfafafaff).into());
    assert_eq!(dark.chat().user_border, rgba(0x333333ff).into());
    assert_eq!(dark.chat().link, rgba(0x6789abff).into());
    assert_eq!(dark.model_picker_bg, rgba(0x345678ff).into());
    assert_eq!(dark.model_picker_accent, rgba(0x6789abff).into());
    let light = colors.tokens(gpui_component::ThemeMode::Light, PaletteScheme::Warp);
    assert_eq!(
        light.window_bg,
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Light, PaletteScheme::Warp)
            .window_bg
    );
    let palette = colors.palette(PaletteScheme::Warp, true);
    let red = terminal::hsla_to_rgb(palette.ansi_colors()[1]);
    assert_eq!((red.r, red.g, red.b), (0xab, 0xcd, 0xef));
    assert_eq!(palette.background(), load_warp_palette(true).background());
    assert_eq!(palette.extended_colors()[1], palette.ansi_colors()[1]);
}

#[test]
fn user_prompt_bubble_colors_follow_theme_mode_and_scheme() {
    let warp_dark =
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Dark, PaletteScheme::Warp);
    assert_eq!(warp_dark.chat().user_background, rgba(0x141414ff).into());
    assert_eq!(warp_dark.chat().user_foreground, rgba(0xffffffff).into());
    assert_eq!(warp_dark.chat().user_border, rgba(0xffffff1a).into());

    let warp_light =
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Light, PaletteScheme::Warp);
    assert_eq!(warp_light.chat().user_background, rgba(0xefefefff).into());
    assert_eq!(warp_light.chat().user_foreground, rgba(0x111111ff).into());
    assert_eq!(warp_light.chat().user_border, rgba(0x1111111a).into());

    let github_dark =
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Dark, PaletteScheme::Github);
    assert!(github_dark.chat().user_background.l < 0.2);
    assert!(github_dark.chat().user_foreground.l > 0.8);

    let github_light =
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Light, PaletteScheme::Github);
    assert!(github_light.chat().user_background.l > 0.8);
    assert!(github_light.chat().user_foreground.l < 0.2);

    let mut colors = ThemeColors::complete_defaults(PaletteScheme::Warp);
    assert_eq!(colors.dark.style["chat.user.bg"], "#141414ff");
    assert_eq!(colors.light.style["chat.user.bg"], "#efefefff");
    colors.change_scheme(PaletteScheme::Warp, PaletteScheme::MidnightGarden);
    assert_eq!(
        colors.dark.style["chat.user.bg"],
        ThemeColors::complete_defaults(PaletteScheme::MidnightGarden)
            .dark
            .style["chat.user.bg"]
    );
}

#[test]
fn invalid_colors_leave_defaults_intact() {
    let colors: ThemeColors = serde_json::from_str(
        r##"{
            "dark": {"ui": {"window_bg": "bad"}, "terminal": {"red": "#12345680"}}
        }"##,
    )
    .unwrap();
    assert_eq!(
        colors
            .tokens(gpui_component::ThemeMode::Dark, PaletteScheme::Warp)
            .window_bg,
        ThemeTokens::from_mode_with_scheme(gpui_component::ThemeMode::Dark, PaletteScheme::Warp)
            .window_bg
    );
    assert_eq!(
        colors.palette(PaletteScheme::Warp, true).ansi_colors()[1],
        load_warp_palette(true).ansi_colors()[1]
    );
}

#[test]
fn test_theme_preference_default_is_system() {
    assert_eq!(ThemePreference::default(), ThemePreference::System);
}

#[test]
fn test_load_github_palette_dark_and_light() {
    let dark_palette = load_github_palette(true);
    let light_palette = load_github_palette(false);
    // Dark background should be very dark, light background very light
    assert!(dark_palette.background().l < 0.2);
    assert!(light_palette.background().l > 0.8);
}

#[test]
fn warp_palette_matches_upstream_colors() {
    let dark = load_warp_palette(true);
    // Warp dark background is #050505, foreground #FFFFFF.
    assert!(dark.background().l < 0.05);
    assert!(dark.foreground().l > 0.95);
    // ANSI red is Warp's #FF8272 and green #B4FA72.
    let ansi = dark.ansi_colors();
    let red = terminal::hsla_to_rgb(ansi[1]);
    assert_eq!((red.r, red.g, red.b), (0xff, 0x82, 0x72));
    let green = terminal::hsla_to_rgb(ansi[2]);
    assert_eq!((green.r, green.g, green.b), (0xb4, 0xfa, 0x72));

    let light = load_warp_palette(false);
    assert!(light.background().l > 0.95);
    assert!(light.foreground().l < 0.2);
}

#[test]
fn warp_scheme_is_default_and_overrides_tokens() {
    assert_eq!(PaletteScheme::default(), PaletteScheme::Warp);
    let mode = gpui_component::ThemeMode::Dark;
    let warp = ThemeTokens::from_mode_with_scheme(mode, PaletteScheme::Warp);
    let github = ThemeTokens::from_mode_with_scheme(mode, PaletteScheme::Github);
    // Warp pane background is near-black; GitHub's is a blue-grey.
    assert!(warp.pane_bg.l < 0.05);
    assert_ne!(warp.pane_bg, github.pane_bg);
    // Warp selection accent is the cyan #19AAD8.
    let accent = terminal::hsla_to_rgb(warp.block_border_selected);
    assert_eq!((accent.r, accent.g, accent.b), (0x19, 0xaa, 0xd8));
    for scheme in [
        PaletteScheme::Warp,
        PaletteScheme::Github,
        PaletteScheme::MidnightGarden,
    ] {
        for (mode, picker_accent) in [
            (gpui_component::ThemeMode::Dark, 0xe38a94ff),
            (gpui_component::ThemeMode::Light, 0xa64c5bff),
        ] {
            let tokens = ThemeColors::complete_defaults(scheme).tokens(mode, scheme);
            assert_eq!(tokens.model_picker_bg, tokens.card_bg);
            assert_eq!(tokens.model_picker_accent, rgba(picker_accent).into());
        }
    }
}

#[test]
fn load_palette_dispatches_by_scheme() {
    let warp = load_palette(PaletteScheme::Warp, true);
    let github = load_palette(PaletteScheme::Github, true);
    assert_eq!(warp.background(), load_warp_palette(true).background());
    assert_eq!(github.background(), load_github_palette(true).background());
}
