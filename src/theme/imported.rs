use super::{PaletteScheme, ThemeTokens, hex_to_rgb};
use gpui::{Hsla, rgba};
use terminal::ColorPalette;

pub(super) struct ImportedTheme {
    surfaces: [u32; 3], // background, panel, element
    text: [u32; 2],     // foreground, muted
    borders: [u32; 3],  // regular, subtle, focus
    primary: [u32; 2],  // fill, foreground on fill
    ansi: [u32; 16],
}

fn color(rgb: u32) -> Hsla {
    rgba((rgb << 8) | 0xff).into()
}

impl ImportedTheme {
    pub(super) fn terminal(&self) -> ColorPalette {
        let (r, g, b) = hex_to_rgb(self.surfaces[0] << 8);
        let builder = ColorPalette::builder().background(r, g, b);
        let (r, g, b) = hex_to_rgb(self.text[0] << 8);
        let builder = builder.foreground(r, g, b);
        let (r, g, b) = hex_to_rgb(self.primary[0] << 8);
        builder
            .cursor(r, g, b)
            .ansi(self.ansi.map(|rgb| hex_to_rgb(rgb << 8)))
            .build()
    }

    pub(super) fn apply(&self, t: &mut ThemeTokens) {
        let [bg, panel, element] = self.surfaces.map(color);
        let [fg, muted] = self.text.map(color);
        let [border, subtle, focus] = self.borders.map(color);
        let [primary, on_primary] = self.primary.map(color);
        let selection = panel.blend(primary.opacity(0.16));
        let hover = panel.blend(fg.opacity(0.07));
        t.window_bg = bg;
        t.text_primary = fg;
        t.text_secondary = muted;
        t.text_muted = muted;
        t.border = border;
        t.border_subtle = subtle;
        t.titlebar_bg = panel;
        t.titlebar_border = subtle;
        t.titlebar_fg = muted;
        t.titlebar_btn_hover = hover;
        t.sidebar_bg = panel;
        t.sidebar_bg_start = panel;
        t.sidebar_bg_end = panel;
        t.sidebar_border = subtle;
        t.sidebar_fg = fg;
        t.sidebar_muted = muted;
        t.sidebar_item_hover = hover;
        t.sidebar_item_active = selection;
        t.sidebar_item_active_fg = fg;
        t.sidebar_section_header = muted;
        t.sidebar_icon_btn_hover = hover;
        t.sidebar_icon = muted;
        t.sidebar_icon_hover = fg;
        t.sidebar_footer_bg = panel;
        t.sidebar_footer_border = subtle;
        t.pane_bg = bg;
        t.pane_border = border;
        t.pane_border_focused = focus;
        t.pane_header_bg = panel;
        t.pane_header_fg = muted;
        t.pane_header_btn_hover = hover;
        t.pane_header_icon = muted;
        t.pane_header_icon_hover = fg;
        t.primary_btn_bg = primary;
        t.primary_btn_hover = primary.blend(on_primary.opacity(0.08));
        t.primary_btn_fg = on_primary;
        t.secondary_btn_bg = element;
        t.secondary_btn_hover = element.blend(fg.opacity(0.07));
        t.secondary_btn_fg = fg;
        t.card_bg = panel;
        t.card_border = border;
        t.card_hover = hover;
        t.pill_active_bg = primary;
        t.pill_active_fg = on_primary;
        t.pill_inactive_bg = element;
        t.pill_inactive_fg = muted;
        t.ring = primary;
        t.block_bg = panel;
        t.block_bg_hover = hover;
        t.block_border = border;
        t.block_border_selected = focus;
        t.block_success = color(self.ansi[2]);
        t.block_failure = color(self.ansi[1]);
        t.block_warning = color(self.ansi[3]);
        t.block_separator = subtle;
        t.block_status_bg = element;
        t.editor_bar_bg = panel;
        t.editor_bar_border = border;
        t.chat_user_bg = element;
        t.chat_user_fg = fg;
        t.chat_user_border = border;
    }
}

// Source palettes and licenses are listed in assets/themes/NOTICE.
pub(super) fn lookup(scheme: PaletteScheme, dark: bool) -> Option<ImportedTheme> {
    use PaletteScheme::*;
    Some(match (scheme, dark) {
        (Cursor, true) => ImportedTheme {
            surfaces: [0x181818, 0x141414, 0x262626],
            text: [0xe4e4e4, 0x7e7e7e],
            borders: [0x272727, 0x0f0f0f, 0x88c0d0],
            primary: [0x88c0d0, 0x000000],
            ansi: [
                0x272727, 0xe34671, 0x3fa266, 0xf1b467, 0x81a1c1, 0xaaa0fa, 0x82d2ce, 0xe4e4e4,
                0x7e7e7e, 0xe3698a, 0x63b182, 0xeebf82, 0x97b0c9, 0xb7aff5, 0x98d6d3, 0xe4e4e4,
            ],
        },
        (Cursor, false) => ImportedTheme {
            surfaces: [0xfcfcfc, 0xf3f3f3, 0xededed],
            text: [0x141414, 0x5f5f5f],
            borders: [0xebebeb, 0xe0e0e0, 0x6e99a4],
            primary: [0x587982, 0xffffff],
            ansi: [
                0xebebeb, 0xcf2d56, 0x1e8461, 0xb15d3f, 0x3b7aa8, 0x6049b3, 0x587982, 0x141414,
                0x5f5f5f, 0xa62847, 0x1c6b50, 0x8e4d36, 0x326487, 0x4f3d90, 0x49636a, 0x141414,
            ],
        },
        (Nord, true) => ImportedTheme {
            surfaces: [0x2e3440, 0x3b4252, 0x434c5e],
            text: [0xeceff4, 0xa6aebd],
            borders: [0x434c5e, 0x434c5e, 0x648093],
            primary: [0x88c0d0, 0x000000],
            ansi: [
                0x434c5e, 0xcc8a92, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
                0xa6aebd, 0xd3a0a8, 0xb3c9a3, 0xebd3a2, 0x99b2cc, 0xc0a3bd, 0xa3c7c8, 0xeceff4,
            ],
        },
        (Nord, false) => ImportedTheme {
            surfaces: [0xeceff4, 0xe5e9f0, 0xd8dee9],
            text: [0x2e3440, 0x3b4252],
            borders: [0x4c566a, 0x4c566a, 0x434c5e],
            primary: [0x526f92, 0xffffff],
            ansi: [
                0x4c566a, 0x9f5761, 0x617161, 0x7c663e, 0x526f92, 0x825982, 0x597076, 0x2e3440,
                0x3b4252, 0x864f5a, 0x56645a, 0x6b5b3e, 0x4a6280, 0x705174, 0x50636a, 0x2e3440,
            ],
        },
        (TokyoNight, true) => ImportedTheme {
            surfaces: [0x1a1b26, 0x1e2030, 0x222436],
            text: [0xc8d3f5, 0x828bb8],
            borders: [0x737aa2, 0x545c7e, 0x9099b2],
            primary: [0x82aaff, 0x000000],
            ansi: [
                0x737aa2, 0xff757f, 0xc3e88d, 0xffc777, 0x82aaff, 0xc099ff, 0x86e1fc, 0xc8d3f5,
                0x828bb8, 0xf38a99, 0xc4e3a4, 0xf3ca93, 0x91b3fd, 0xc2a6fd, 0x95defa, 0xc8d3f5,
            ],
        },
        (TokyoNight, false) => ImportedTheme {
            surfaces: [0xe1e2e7, 0xd5d6db, 0xc8c9ce],
            text: [0x3760bf, 0x515b79],
            borders: [0x737a8c, 0x9699a8, 0x5a607d],
            primary: [0x3760bf, 0xffffff],
            ansi: [
                0x737a8c, 0x9a4490, 0x446889, 0x7e5e50, 0x3760bf, 0x7741b8, 0x006485, 0x3760bf,
                0x515b79, 0x844a9a, 0x416695, 0x6e5e68, 0x3760bf, 0x6948ba, 0x0c6392, 0x3760bf,
            ],
        },
        (Vercel, true) => ImportedTheme {
            surfaces: [0x000000, 0x1a1a1a, 0x292929],
            text: [0xededed, 0x878787],
            borders: [0x1f1f1f, 0x1a1a1a, 0x235a9a],
            primary: [0x0070f3, 0x000000],
            ansi: [
                0x1f1f1f, 0xe5484d, 0x46a758, 0xffb224, 0x52a8ff, 0x9559c9, 0x50e3c2, 0xededed,
                0x878787, 0xe76c70, 0x6bb679, 0xfbbf50, 0x74b7fb, 0xa87ad1, 0x73e5cb, 0xededed,
            ],
        },
        (Vercel, false) => ImportedTheme {
            surfaces: [0xffffff, 0xfafafa, 0xeaeaea],
            text: [0x171717, 0x666666],
            borders: [0xeaeaea, 0xeaeaea, 0x8b95a1],
            primary: [0x0070f3, 0x000000],
            ansi: [
                0xeaeaea, 0xdc3545, 0x368639, 0xa96609, 0x0070f3, 0x8e4ec6, 0x157d70, 0x171717,
                0x666666, 0xb12e3b, 0x2f6e32, 0x89550c, 0x055cc3, 0x7442a0, 0x15675c, 0x171717,
            ],
        },
        (Dracula, true) => ImportedTheme {
            surfaces: [0x282a36, 0x21222c, 0x44475a],
            text: [0xf8f8f2, 0x7c89b1],
            borders: [0x44475a, 0x191a21, 0xbd93f9],
            primary: [0xbd93f9, 0x000000],
            ansi: [
                0x44475a, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                0x7c89b1, 0xfd7978, 0x75fa95, 0xf3faa2, 0xcaa9f7, 0xfd95d0, 0xa3ecfb, 0xf8f8f2,
            ],
        },
        (Dracula, false) => ImportedTheme {
            surfaces: [0xf8f8f2, 0xe8e8e2, 0xd8d8d2],
            text: [0x282a36, 0x596794],
            borders: [0xc8c8c2, 0xe0e0e0, 0xa180d5],
            primary: [0x7d66a5, 0xffffff],
            ansi: [
                0xc8c8c2, 0xc3494c, 0x387f52, 0x707555, 0x7d66a5, 0xa5588a, 0x507686, 0x282a36,
                0x596794, 0xa14247, 0x346c4c, 0x60644e, 0x6a598d, 0x8a4e78, 0x476574, 0x282a36,
            ],
        },
        (Matrix, true) => ImportedTheme {
            surfaces: [0x0a0e0a, 0x0e130d, 0x141c12],
            text: [0x62ff94, 0x8ca391],
            borders: [0x1e2a1b, 0x141c12, 0x2eff6a],
            primary: [0x2eff6a, 0x000000],
            ansi: [
                0x1e2a1b, 0xff4b4b, 0x62ff94, 0xe6ff57, 0x30b3ff, 0xc770ff, 0x00efff, 0x62ff94,
                0x8ca391, 0xdc735b, 0x62ff94, 0xc9ff64, 0x3bc4e7, 0xb18fe7, 0x16f3e7, 0x62ff94,
            ],
        },
        (Matrix, false) => ImportedTheme {
            surfaces: [0xeef3ea, 0xe4ebe1, 0xdae1d7],
            text: [0x203022, 0x5d6d5f],
            borders: [0x748476, 0xdae1d7, 0x1da042],
            primary: [0x1e7d38, 0xffffff],
            ansi: [
                0x748476, 0xbe433f, 0x1e7d38, 0x617433, 0x287597, 0x8958ad, 0x117980, 0x203022,
                0x5d6d5f, 0x9b3f39, 0x1e6c33, 0x53652f, 0x26667d, 0x724f8e, 0x14696b, 0x203022,
            ],
        },
        (NightOwl, true) => ImportedTheme {
            surfaces: [0x011627, 0x0b253a, 0x0b253a],
            text: [0xd6deeb, 0x728da4],
            borders: [0x5f7e97, 0x5f7e97, 0x82aaff],
            primary: [0x82aaff, 0x000000],
            ansi: [
                0x5f7e97, 0xef5350, 0xc5e478, 0xecc48d, 0x82aaff, 0xc792ea, 0x7fdbca, 0xd6deeb,
                0x728da4, 0xea7272, 0xc9e391, 0xe7caa2, 0x94b5fb, 0xcaa3ea, 0x92dcd1, 0xd6deeb,
            ],
        },
        (NightOwl, false) => ImportedTheme {
            surfaces: [0xfbfbfb, 0xf0f0f0, 0xf0f0f0],
            text: [0x403f53, 0x646e7e],
            borders: [0xd9d9d9, 0xd9d9d9, 0x7896a6],
            primary: [0x2e79b3, 0xffffff],
            ansi: [
                0xd9d9d9, 0xd33d3d, 0x128366, 0x88712f, 0x2e79b3, 0xc44383, 0x327f80, 0x403f53,
                0x646e7e, 0xb33d42, 0x1c7462, 0x786637, 0x326c9e, 0xa74278, 0x357176, 0x403f53,
            ],
        },
        (Orng, true) => ImportedTheme {
            surfaces: [0x0a0a0a, 0x141414, 0x1e1e1e],
            text: [0xeeeeee, 0x808080],
            borders: [0xec5b2b, 0x3c3c3c, 0xee7948],
            primary: [0xec5b2b, 0x000000],
            ansi: [
                0x3c3c3c, 0xe06c75, 0x6ba1e6, 0xe5c07b, 0x6ba1e6, 0xfa9f76, 0x56b6c2, 0xeeeeee,
                0x808080, 0xe38990, 0x88b2e8, 0xe7ca94, 0x88b2e8, 0xf7b08f, 0x77c2cc, 0xeeeeee,
            ],
        },
        (Orng, false) => ImportedTheme {
            surfaces: [0xffffff, 0xfff7f1, 0xf5f0eb],
            text: [0x1a1a1a, 0x727272],
            borders: [0xec5b2b, 0xd4d4d4, 0xc94d24],
            primary: [0xc85028, 0x000000],
            ansi: [
                0xd4d4d4, 0xd1383d, 0x0062d1, 0x926717, 0x0062d1, 0xc94d24, 0x30808e, 0x1a1a1a,
                0x727272, 0xa93135, 0x0652a9, 0x78561a, 0x0652a9, 0xa24222, 0x2b6a74, 0x1a1a1a,
            ],
        },
        (Vesper, true) => ImportedTheme {
            surfaces: [0x101010, 0x101010, 0x101010],
            text: [0xffffff, 0xa0a0a0],
            borders: [0x282828, 0x1c1c1c, 0xffc799],
            primary: [0xffc799, 0x000000],
            ansi: [
                0x282828, 0xff8080, 0x99ffe4, 0xffc799, 0xffc799, 0xffc799, 0x99ffe4, 0xffffff,
                0xa0a0a0, 0xff9c9c, 0xafffea, 0xffd3af, 0xffd3af, 0xffd3af, 0xafffea, 0xffffff,
            ],
        },
        (Vesper, false) => ImportedTheme {
            surfaces: [0xffffff, 0xf0f0f0, 0xe0e0e0],
            text: [0x101010, 0x6c6c6c],
            borders: [0xd0d0d0, 0xe8e8e8, 0xb38d6d],
            primary: [0x8c6f57, 0xffffff],
            ansi: [
                0xd0d0d0, 0xb35c5c, 0x4f7e72, 0x8c6f57, 0x8c6f57, 0x8c6f57, 0x4f7e72, 0x101010,
                0x6c6c6c, 0x8f4b4b, 0x41665c, 0x715a47, 0x715a47, 0x715a47, 0x41665c, 0x101010,
            ],
        },
        (Shob | Github | MidnightGarden, _) => return None,
    })
}
