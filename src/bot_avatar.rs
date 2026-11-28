//! The supplied avatar studio's SVG art and a native driver for its three motion rigs.
use gpui::{
    App, AppContext as _, Bounds, Context, Corners, Element, ElementId, Entity, Global,
    GlobalElementId, Hsla, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    ParentElement, Pixels, Point, Render, RenderImage, RenderOnce, StatefulInteractiveElement as _,
    Style, StyleRefinement, Styled as _, Window, div, point, px, rgb, size,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashMap,
    f32::consts::{PI, TAU},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

const VIEWBOX_SIZE: f32 = 316.0;

macro_rules! presets {
    ($name:ident { $($(#[$attr:meta])* $variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub(crate) enum $name {
            $($(#[$attr])* $variant),+
        }

        impl $name {
            pub(crate) const ALL: [Self; <[&str]>::len(&[$(stringify!($variant)),+])] = [$(Self::$variant),+];

            pub(crate) const fn label(self) -> &'static str {
                match self { $(Self::$variant => stringify!($variant)),+ }
            }
        }
    };
}

// The custom defaults and preset ordering match the studio.
presets!(Palette {
    Obsidian, Espresso, Coral, Orange, Amber, Mint, #[default] Blue, Indigo, Violet, Pink, Cyan, Pearl,
});

struct Colors {
    swatch: u32,
    stops: [u32; 4],
    eyes: u32,
}

impl Palette {
    pub(crate) fn swatch(self) -> Hsla {
        rgb(self.colors().swatch).into()
    }

    fn colors(self) -> Colors {
        let (swatch, stops, eyes) = match self {
            Self::Obsidian => (0x18181b, [0x52525b, 0x27272a, 0x141417, 0x09090b], 0xf4f4f5),
            Self::Espresso => (0x8a6240, [0xd6b28d, 0xa4754d, 0x744e30, 0x4f321c], 0x0a0a0c),
            Self::Coral => (0xef4d4d, [0xffaca8, 0xf7635e, 0xe53e3e, 0xb8242a], 0x0a0a0c),
            Self::Orange => (0xf98a22, [0xffd39c, 0xff9e3d, 0xef7812, 0xc25506], 0x0a0a0c),
            Self::Amber => (0xeab326, [0xfef0a5, 0xfacc38, 0xe5a310, 0xb87904], 0x0a0a0c),
            Self::Mint => (0x3cd085, [0xb3f7d1, 0x54e098, 0x27bf72, 0x139453], 0x0a0a0c),
            Self::Blue => (0x4398ff, [0xc4e2ff, 0x6db4ff, 0x3b92fc, 0x1c6ee8], 0x08080a),
            Self::Indigo => (0x6366f1, [0xc7d2fe, 0x818cf8, 0x585bf0, 0x3b38c9], 0x08080a),
            Self::Violet => (0xa855f7, [0xe9d5ff, 0xc084fc, 0x9b4ef5, 0x7628d1], 0x08080a),
            Self::Pink => (0xf472b6, [0xfbcfe8, 0xf78ac3, 0xea58a3, 0xc4307d], 0x08080a),
            Self::Cyan => (0x22d3ee, [0xcffafe, 0x5ce1f5, 0x14bdd8, 0x088da8], 0x08080a),
            Self::Pearl => (0xe4e4e7, [0xffffff, 0xf4f4f5, 0xd4d4d8, 0xa1a1aa], 0x09090b),
        };
        Colors { swatch, stops, eyes }
    }
}

presets!(Shape {
    #[default] Circle, Pebble, Squircle, Capsule, Triangle, Hexagon, Cloud, Droplet,
});

impl Shape {
    fn geometry(self) -> (&'static str, f32) {
        match self {
            Self::Circle => ("M 0,-118 A 118,118 0 1,0 0,118 A 118,118 0 1,0 0,-118 Z", -2.0),
            Self::Pebble => ("M -8,-112 C 62,-114 122,-72 118,8 C 114,82 54,116 -18,112 C -88,108 -124,62 -118,-10 C -112,-76 -72,-110 -8,-112 Z", 4.0),
            Self::Squircle => ("M 0,-114 C 92,-114 114,-92 114,0 C 114,92 92,114 0,114 C -92,114 -114,92 -114,0 C -114,-92 -92,-114 0,-114 Z", 0.0),
            Self::Capsule => ("M -46,-84 L 46,-84 A 84,84 0 0,1 46,84 L -46,84 A 84,84 0 0,1 -46,-84 Z", 0.0),
            Self::Triangle => ("M -24,-96 Q 0,-132 24,-96 L 108,58 Q 130,98 84,98 L -84,98 Q -130,98 -108,58 Z", 24.0),
            Self::Hexagon => ("M -48,-106 L 48,-106 Q 68,-106 78,-88 L 122,-16 Q 132,0 122,16 L 78,88 Q 68,106 48,106 L -48,106 Q -68,106 -78,88 L -122,16 Q -132,0 -122,-16 L -78,-88 Q -68,-106 -48,-106 Z", 0.0),
            Self::Cloud => ("M -66,-22 C -68,-74 -20,-104 20,-80 C 56,-102 104,-68 96,-18 C 132,-4 134,58 96,80 C 78,94 42,96 0,94 C -44,96 -80,94 -98,78 C -134,54 -128,-10 -86,-22 Z", 10.0),
            Self::Droplet => ("M 0,-124 C 14,-100 104,-14 104,36 A 104,104 0 0,1 -104,36 C -104,-14 -14,-100 0,-124 Z", 24.0),
        }
    }
}

presets!(Face {
    Neutral, Attentive, #[default] Surprised, Excited, Happy, Laughing, Angry, Sad,
    Scared, Suspicious, Confused, Curious, Proud, Shy, Unimpressed, Sleepy,
});

impl Face {
    fn geometry(self) -> &'static str {
        match self {
            Self::Neutral => r#"<ellipse cx="-26" cy="-4" rx="11" ry="18"/>
                <ellipse cx="26" cy="-4" rx="11" ry="18"/>"#,
            Self::Attentive => r#"<rect x="-38" y="-26" width="22" height="44" rx="11"/>
                <rect x="16" y="-26" width="22" height="44" rx="11"/>"#,
            Self::Surprised => r#"<ellipse cx="-34" cy="-2" rx="23" ry="28"/>
                <ellipse cx="34" cy="-2" rx="23" ry="28"/>"#,
            Self::Excited => r##"<ellipse cx="-32" cy="-2" rx="19" ry="29"/>
                <ellipse cx="32" cy="-2" rx="19" ry="29"/>
                <circle cx="-38" cy="-12" r="5.5" fill="#ffffff" fill-opacity="0.85"/>
                <circle cx="26" cy="-12" r="5.5" fill="#ffffff" fill-opacity="0.85"/>"##,
            Self::Happy => r#"<path d="M -46,2 Q -30,-20 -14,2 M 14,2 Q 30,-20 46,2"
                fill="none" stroke="currentColor" stroke-width="11" stroke-linecap="round"/>"#,
            Self::Laughing => r#"<path d="M -48,4 Q -30,-16 -12,4 M 12,4 Q 30,-16 48,4"
                fill="none" stroke="currentColor" stroke-width="11" stroke-linecap="round"/>
                <path d="M -16,22 Q 0,38 16,22 Z"/>"#,
            Self::Angry => r#"<path d="M -48,-12 L -14,0 A 15,15 0 1,1 -44,6 Z"/>
                <path d="M 48,-12 L 14,0 A 15,15 0 1,0 44,6 Z"/>"#,
            Self::Sad => r#"<ellipse cx="-30" cy="4" rx="14" ry="21" transform="rotate(-14 -30 0)"/>
                <ellipse cx="30" cy="4" rx="14" ry="21" transform="rotate(14 30 0)"/>
                <path d="M -48,-18 Q -32,-26 -16,-14 M 48,-18 Q 32,-26 16,-14"
                    fill="none" stroke="currentColor" stroke-width="6" stroke-linecap="round"/>"#,
            Self::Scared => r##"<ellipse cx="-30" cy="4" rx="17" ry="26"/>
                <ellipse cx="30" cy="4" rx="17" ry="26"/>
                <circle cx="-30" cy="4" r="5" fill="#ffffff" fill-opacity="0.7"/>
                <circle cx="30" cy="4" r="5" fill="#ffffff" fill-opacity="0.7"/>"##,
            Self::Suspicious => r#"<rect x="-48" y="-8" width="32" height="12" rx="6"/>
                <ellipse cx="30" cy="-2" rx="13" ry="19"/>"#,
            Self::Confused => r#"<ellipse cx="-30" cy="-10" rx="17" ry="22"/>
                <circle cx="30" cy="4" r="11"/>"#,
            Self::Curious => r#"<ellipse cx="-24" cy="-6" rx="14" ry="24"/>
                <ellipse cx="30" cy="-2" rx="11" ry="18"/>"#,
            Self::Proud => r#"<path d="M -46,-4 Q -30,-16 -14,-4 Q -30,6 -46,-4 Z"/>
                <path d="M 14,-4 Q 30,-16 46,-4 Q 30,6 14,-4 Z"/>"#,
            Self::Shy => r##"<ellipse cx="-26" cy="8" rx="13" ry="17"/>
                <ellipse cx="26" cy="8" rx="13" ry="17"/>
                <ellipse cx="-52" cy="24" rx="13" ry="7" fill="#ff5a8a" fill-opacity="0.45"/>
                <ellipse cx="52" cy="24" rx="13" ry="7" fill="#ff5a8a" fill-opacity="0.45"/>"##,
            Self::Unimpressed => r#"<rect x="-48" y="-6" width="32" height="11" rx="5.5"/>
                <rect x="16" y="-6" width="32" height="11" rx="5.5"/>"#,
            Self::Sleepy => r#"<path d="M -46,2 Q -30,18 -14,2 M 14,2 Q 30,18 46,2"
                fill="none" stroke="currentColor" stroke-width="10" stroke-linecap="round"/>"#,
        }
    }
}

presets!(Motion { #[default] Idle, Listening, Thinking, Speaking });
presets!(Interaction { #[default] Look, Move });

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct AvatarConfig {
    pub(crate) palette: Palette,
    pub(crate) shape: Shape,
    pub(crate) face: Face,
    pub(crate) motion: Motion,
    pub(crate) interaction: Interaction,
}

fn body_svg(shape: Shape, palette: Palette) -> String {
    let [s0, s1, s2, s3] = palette.colors().stops;
    let (path, _) = shape.geometry();
    format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-158 -158 316 316" width="316" height="316">
        <defs>
          <radialGradient id="body" cx="36%" cy="28%" r="74%" fx="30%" fy="20%">
            <stop offset="0%" stop-color="#{s0:06x}"/><stop offset="32%" stop-color="#{s1:06x}"/>
            <stop offset="72%" stop-color="#{s2:06x}"/><stop offset="100%" stop-color="#{s3:06x}"/>
          </radialGradient>
          <radialGradient id="sheen" cx="32%" cy="22%" r="44%">
            <stop offset="0%" stop-color="#ffffff" stop-opacity="0.34"/>
            <stop offset="55%" stop-color="#ffffff" stop-opacity="0.06"/>
            <stop offset="100%" stop-color="#ffffff" stop-opacity="0"/>
          </radialGradient>
        </defs>
        <path d="{path}" fill="url(#body)"/><path d="{path}" fill="url(#sheen)"/>
      </svg>"##)
}

fn face_svg(face: Face, color: u32) -> String {
    let geometry = face.geometry();
    format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-72 -40 144 80" width="144" height="80">
        <g color="#{color:06x}" fill="#{color:06x}">{geometry}</g>
      </svg>"##)
}

const FACE_WIDTH: f32 = 144.0;
const FACE_HEIGHT: f32 = 80.0;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Resolution {
    Thumbnail,
    Preview,
}

#[derive(Clone)]
struct Layers {
    body: Arc<RenderImage>,
    face: Arc<RenderImage>,
}

// The keys are finite and both layers are shared independently across bots. A
// blink or movement only changes paint coordinates, never the uploaded images.
#[derive(Default)]
struct CachedLayers {
    bodies: HashMap<(Resolution, Shape, Palette), Arc<RenderImage>>,
    faces: HashMap<(Resolution, Face, u32), Arc<RenderImage>>,
}

#[derive(Default)]
struct AvatarImages(RefCell<CachedLayers>);

impl Global for AvatarImages {}

fn layers(config: AvatarConfig, resolution: Resolution, cx: &mut App) -> Layers {
    if !cx.has_global::<AvatarImages>() {
        cx.set_global(AvatarImages::default());
    }
    // Cache fills do not notify global observers or invalidate other views.
    let mut cache = cx.global::<AvatarImages>().0.borrow_mut();
    let logical_width = match resolution {
        Resolution::Thumbnail => 32.0,
        Resolution::Preview => f32::from(AvatarPreview::SIZE),
    };
    // GPUI rasterizes at twice the requested size. Both layers use the same
    // source-pixel scale, even though the face has a smaller viewBox.
    let scale = logical_width / VIEWBOX_SIZE;
    let body = cache
        .bodies
        .entry((resolution, config.shape, config.palette))
        .or_insert_with(|| {
            cx.svg_renderer()
                .render_single_frame(body_svg(config.shape, config.palette).as_bytes(), scale)
                .expect("built-in avatar body SVG")
        })
        .clone();
    let eye_color = config.palette.colors().eyes;
    let face = cache
        .faces
        .entry((resolution, config.face, eye_color))
        .or_insert_with(|| {
            cx.svg_renderer()
                .render_single_frame(face_svg(config.face, eye_color).as_bytes(), scale)
                .expect("built-in avatar face SVG")
        })
        .clone();
    Layers { body, face }
}

#[derive(Clone, Copy)]
struct Pose {
    body_shift: [f32; 2],
    body_scale: [f32; 2],
    gaze: [f32; 2],
    blink: f32,
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            body_shift: [0.0, 0.0],
            body_scale: [1.0, 1.0],
            gaze: [0.0, 0.0],
            blink: 0.0,
        }
    }
}

fn paint_art(bounds: Bounds<Pixels>, layers: Layers, eye_y: f32, pose: Pose, window: &mut Window) {
    let unit = bounds.size.width.min(bounds.size.height) / VIEWBOX_SIZE;
    let body_center = bounds.center()
        + point(unit * pose.body_shift[0], unit * pose.body_shift[1]);
    let ux = unit * pose.body_scale[0];
    let uy = unit * pose.body_scale[1];
    let body = Bounds::centered_at(body_center, size(ux * VIEWBOX_SIZE, uy * VIEWBOX_SIZE));
    let face = Bounds::centered_at(
        body_center + point(ux * pose.gaze[0], uy * (eye_y + pose.gaze[1])),
        size(
            ux * FACE_WIDTH * (1.0 + 0.08 * pose.blink),
            uy * FACE_HEIGHT * (1.0 - 0.94 * pose.blink),
        ),
    );
    for (bounds, image) in [(body, layers.body), (face, layers.face)] {
        let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
    }
}

struct Blink {
    started_at: f32,
    duration: f32,
    double: bool,
}

struct Life {
    last_frame: Option<Instant>,
    elapsed: f32,
    last_mouse: Option<Point<Pixels>>,
    pointer_until: f32,
    pointer_target: [f32; 2],
    auto_target: [f32; 2],
    next_gaze: f32,
    gaze: [f32; 2],
    blink: Option<Blink>,
    next_blink: f32,
    next_blink_is_followup: bool,
    pop: f32,
    seed: u32,
}

impl Life {
    fn new() -> Self {
        static NEXT_SEED: AtomicU32 = AtomicU32::new(0xa4cb_7281);
        Self {
            last_frame: None,
            elapsed: 0.0,
            last_mouse: None,
            pointer_until: 0.0,
            pointer_target: [0.0, 0.0],
            auto_target: [0.0, 0.0],
            next_gaze: 1.2,
            gaze: [0.0, 0.0],
            blink: None,
            next_blink: 1.6,
            next_blink_is_followup: false,
            pop: 0.0,
            seed: NEXT_SEED.fetch_add(0x9e37_79b9, Ordering::Relaxed) | 1,
        }
    }

    fn random(&mut self) -> f32 {
        // A local xorshift generator is sufficient for organic timing; no
        // shared random state is accessed on animation frames.
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed >> 8) as f32 / 16_777_216.0
    }

    fn pause(&mut self) {
        self.last_frame = None;
        self.last_mouse = None;
        self.pointer_until = 0.0;
    }

    fn track_pointer(&mut self, mouse: Option<Point<Pixels>>, center: Point<Pixels>) {
        if let Some(position) = mouse {
            if self.last_mouse.is_some_and(|last| last != position) {
                let offset = position - center;
                let (x, y) = (f32::from(offset.x), f32::from(offset.y));
                let divisor = x.hypot(y).max(260.0);
                self.pointer_target = [x / divisor, y / divisor];
                self.pointer_until = self.elapsed + 1.8;
            }
        } else {
            self.pointer_until = 0.0;
        }
        self.last_mouse = mouse;
    }

    fn saccade(&mut self, motion: Motion) {
        let (target, minimum, spread) = match motion {
            Motion::Thinking => ([(self.random() - 0.5) * 1.4, -0.65 - self.random() * 0.3], 1.4, 1.4),
            Motion::Listening => ([(self.random() - 0.5) * 0.45, (self.random() - 0.5) * 0.3], 1.5, 1.5),
            Motion::Speaking => ([(self.random() - 0.5) * 0.8, -0.15 + (self.random() - 0.5) * 0.35], 0.65, 0.9),
            Motion::Idle => {
                let target = if self.random() < 0.4 {
                    [0.0, 0.0]
                } else {
                    let (sin, cos) = (self.random() * TAU).sin_cos();
                    let r = 0.35 + self.random() * 0.55;
                    [cos * r, sin * r * 0.72]
                };
                (target, 1.1, 2.1)
            }
        };
        self.auto_target = target;
        self.next_gaze = self.elapsed + minimum + self.random() * spread;
    }

    fn start_blink(&mut self, allow_double: bool) {
        let duration = 0.135 + self.random() * 0.045;
        let double = allow_double && self.random() < 0.24;
        self.blink = Some(Blink { started_at: self.elapsed, duration, double });
    }

    fn blink_amount(&mut self, motion: Motion) -> f32 {
        if self.blink.is_none() && self.elapsed >= self.next_blink {
            self.start_blink(!self.next_blink_is_followup);
        }
        let Some(blink) = self.blink.as_ref() else { return 0.0 };
        let progress = (self.elapsed - blink.started_at) / blink.duration;
        if progress < 1.0 {
            return (progress * PI).sin();
        }
        let (minimum, spread) = if blink.double {
            (0.11, 0.065)
        } else {
            match motion {
                Motion::Speaking => (1.4, 2.0),
                Motion::Thinking => (2.5, 2.5),
                _ => (1.8, 3.0),
            }
        };
        self.next_blink_is_followup = blink.double;
        self.next_blink = self.elapsed + minimum + self.random() * spread;
        self.blink = None;
        0.0
    }

    fn tick(
        &mut self,
        config: AvatarConfig,
        now: Instant,
        mouse: Option<Point<Pixels>>,
        center: Point<Pixels>,
    ) -> Pose {
        let dt = self
            .last_frame
            .replace(now)
            .map_or(0.0, |last| now.saturating_duration_since(last).as_secs_f32());
        self.elapsed += dt;
        self.track_pointer(mouse, center);
        let cursor_live = self.elapsed < self.pointer_until;
        if !cursor_live && self.elapsed >= self.next_gaze {
            self.saccade(config.motion);
        }
        let target = if cursor_live { self.pointer_target } else { self.auto_target };
        // Keep the feel of the studio's 60 Hz lerp on both slower and faster displays.
        let residual: f32 = if cursor_live { 0.84 } else { 0.915 };
        let lerp = 1.0 - residual.powf(dt * 60.0);
        for (current, target) in self.gaze.iter_mut().zip(target) {
            *current += (target - *current) * lerp;
        }
        self.pop *= 0.86_f32.powf(dt * 60.0);

        let t = self.elapsed;
        let (mut body_shift, mut body_scale, eye_micro_y) = match config.motion {
            Motion::Idle => {
                let breath = (t * 1.65).sin();
                ([(t * 0.75).sin() * 2.2, breath * 5.2], [1.0 + breath * 0.015, 1.0 - breath * 0.015], 0.0)
            }
            Motion::Listening => {
                let nod = (t * 2.4).sin().max(0.0).powi(3);
                ([0.0, nod * 6.5 - 2.0], [1.0 + nod * 0.025, 1.0 - nod * 0.025], nod * 2.2)
            }
            Motion::Thinking => {
                let drift = (t * 1.8).sin() * 0.014;
                ([(t * 1.2).sin() * 5.5, (t * 2.4).sin() * 3.5 - 5.0], [1.0 + drift, 1.0 - drift], 0.0)
            }
            Motion::Speaking => {
                let pulse = ((t * 9.5).sin() * 0.65 + (t * 14.2).sin() * 0.35).max(0.0);
                ([(t * 3.2).sin() * 2.8, -pulse * 9.5], [1.0 - pulse * 0.045, 1.0 + pulse * 0.055], -pulse * 2.4)
            }
        };
        if config.interaction == Interaction::Move && cursor_live {
            body_shift[0] += self.gaze[0] * 14.0;
            body_shift[1] += self.gaze[1] * 11.0;
        }
        body_scale[0] += self.pop * 0.08;
        body_scale[1] -= self.pop * 0.08;

        // GPUI's color sprites can translate and scale, but cannot rotate.
        Pose {
            body_shift,
            body_scale,
            gaze: [self.gaze[0] * 18.0, self.gaze[1] * 14.0 + eye_micro_y],
            blink: self.blink_amount(config.motion),
        }
    }
}

pub(crate) struct AvatarPreview {
    config: AvatarConfig,
    life: Rc<RefCell<Life>>,
    images: Option<Layers>,
}

impl AvatarPreview {
    pub(crate) const SIZE: Pixels = px(112.0);

    pub(crate) fn new(config: AvatarConfig) -> Self {
        Self { config, life: Rc::new(RefCell::new(Life::new())), images: None }
    }

    pub(crate) fn set_config(&mut self, config: AvatarConfig) {
        {
            let mut life = self.life.borrow_mut();
            if self.config.motion != config.motion {
                life.pop = 0.8;
                life.start_blink(false);
            } else if self.config.face != config.face {
                life.pop = 0.7;
                life.start_blink(false);
            } else if self.config.shape != config.shape {
                life.pop = 0.7;
            }
        }
        if self.config.palette != config.palette
            || self.config.shape != config.shape
            || self.config.face != config.face
        {
            self.images = None;
        }
        self.config = config;
    }
}

impl Render for AvatarPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let images = self.images.get_or_insert_with(|| layers(self.config, Resolution::Preview, cx));
        AvatarArt {
            config: self.config,
            width: Self::SIZE,
            source: ArtSource::Preview { life: self.life.clone(), images: images.clone() },
        }
    }
}

enum ArtSource {
    Thumbnail { pose: Pose, animate: bool },
    Preview { life: Rc<RefCell<Life>>, images: Layers },
}

struct AvatarArt {
    config: AvatarConfig,
    width: Pixels,
    source: ArtSource,
}

impl Element for AvatarArt {
    type RequestLayoutState = ();
    type PrepaintState = Option<(Layers, Pose, bool)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(self.width.into(), self.width.into()),
            flex_shrink: 0.0,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let visible = !bounds.intersect(&window.content_mask().bounds).is_empty();
        if !visible {
            if let ArtSource::Preview { life, .. } = &self.source {
                life.borrow_mut().pause();
            }
            return None;
        }
        let active = window.is_window_active() && !cx.reduce_motion();
        match &self.source {
            ArtSource::Thumbnail { pose, animate } => Some((
                layers(self.config, Resolution::Thumbnail, cx),
                *pose,
                active && *animate,
            )),
            ArtSource::Preview { life, images } => {
                let mut life = life.borrow_mut();
                let pose = if active {
                    life.tick(
                        self.config,
                        cx.background_executor().now(),
                        window.is_window_hovered().then(|| window.mouse_position()),
                        bounds.center(),
                    )
                } else {
                    life.pause();
                    Pose::default()
                };
                Some((images.clone(), pose, active))
            }
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        if let Some((images, pose, animate)) = frame.take() {
            paint_art(bounds, images, self.config.shape.geometry().1, pose, window);
            if animate {
                window.request_animation_frame();
            }
        }
    }
}

pub(crate) fn avatar(config: AvatarConfig, width: Pixels) -> impl IntoElement {
    thumbnail(config, width, Pose::default(), false)
}

fn thumbnail(config: AvatarConfig, width: Pixels, pose: Pose, animate: bool) -> AvatarArt {
    AvatarArt { config, width, source: ArtSource::Thumbnail { pose, animate } }
}

pub(crate) fn sidebar_avatar(
    id: impl Into<ElementId>,
    config: AvatarConfig,
    width: Pixels,
) -> impl IntoElement {
    SidebarAvatar { id: id.into(), config, width }
}

#[derive(IntoElement)]
struct SidebarAvatar {
    id: ElementId,
    config: AvatarConfig,
    width: Pixels,
}

impl RenderOnce for SidebarAvatar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, config, width } = self;
        // Keep the element-scoped entity without observing its changes from
        // the sidebar's parent. Only its retained child repaints on hover.
        let avatar = window.with_global_id(id, |global_id, window| {
            window.with_element_state(
                global_id,
                |state: Option<(AvatarConfig, Pixels, Entity<HoverAvatar>)>, _| {
                    let avatar = match state {
                        Some((old_config, old_width, avatar)) => {
                            if old_config != config || old_width != width {
                                avatar.update(cx, |state, cx| {
                                    state.config = config;
                                    state.width = width;
                                    cx.notify();
                                });
                            }
                            avatar
                        }
                        None => cx.new(|_| HoverAvatar { config, width, start: None }),
                    };
                    (avatar.clone(), (config, width, avatar))
                },
            )
        });
        avatar.cached(StyleRefinement::default().size(width).flex_shrink_0())
    }
}

struct HoverAvatar {
    config: AvatarConfig,
    width: Pixels,
    start: Option<Instant>,
}

impl Render for HoverAvatar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        const DURATION: f32 = 0.34;
        let active = window.is_window_active() && !cx.reduce_motion();
        let elapsed = self.start
            .filter(|_| active)
            .map(|start| cx.background_executor().now().saturating_duration_since(start).as_secs_f32())
            .filter(|elapsed| *elapsed < DURATION);
        let art = if let Some(elapsed) = elapsed {
            let bounce = (elapsed / DURATION * PI).sin();
            let blink = ((elapsed - 0.08) / 0.155).clamp(0.0, 1.0);
            thumbnail(self.config, self.width, Pose {
                body_shift: [0.0, -8.0 * bounce],
                body_scale: [1.0 + 0.025 * bounce, 1.0 - 0.025 * bounce],
                blink: (PI * blink).sin(),
                ..Pose::default()
            }, true)
        } else {
            self.start = None;
            thumbnail(self.config, self.width, Pose::default(), false)
        };

        div()
            .id("hover-bot-avatar")
            .size(self.width)
            .flex_shrink_0()
            .on_hover(cx.listener(|this, hovering: &bool, window, cx| {
                if *hovering && window.is_window_active() && !cx.reduce_motion() {
                    this.start = Some(cx.background_executor().now());
                    cx.notify();
                }
            }))
            .child(art)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::SvgRenderer;

    #[test]
    fn bot_avatar_presets_render_pixels() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let palette = Palette::default();
        for svg in Shape::ALL
            .into_iter()
            .map(|shape| body_svg(shape, palette))
            .chain(Face::ALL.into_iter().map(|face| face_svg(face, palette.colors().eyes)))
        {
            let image = renderer.render_single_frame(svg.as_bytes(), 0.125).unwrap();
            let pixels = image.as_bytes(0).unwrap();
            assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 255), "{svg}");
            assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0), "{svg}");
        }
    }

    #[test]
    fn bot_avatar_config_round_trips() {
        let config = AvatarConfig {
            palette: Palette::Violet,
            shape: Shape::Squircle,
            face: Face::Proud,
            motion: Motion::Thinking,
            interaction: Interaction::Move,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<AvatarConfig>(&json).unwrap(), config);
        assert_eq!(serde_json::from_str::<AvatarConfig>("{}").unwrap(), AvatarConfig::default());
    }
}
