fn chat_output_expander(
    id: SharedString,
    hidden_bytes: usize,
    show_full: bool,
    tokens: ThemeTokens,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = if show_full {
        "Show less".to_string()
    } else {
        format!(
            "Show all output ({} KiB hidden)",
            hidden_bytes.div_ceil(1024).max(1)
        )
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .px(px(12.0))
        .py(px(5.0))
        .cursor(CursorStyle::PointingHand)
        .text_size(px(11.0))
        .text_color(tokens.text_secondary)
        .hover(move |style| style.text_color(tokens.text_primary))
        .on_mouse_down(MouseButton::Left, on_toggle)
        .child(label)
}

#[derive(IntoElement)]
struct ToolPlainText {
    id: SharedString,
    text: SharedString,
}

impl gpui::RenderOnce for ToolPlainText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        TextView::plain(self.id, self.text, window, cx)
            .selectable(true)
            .w_full()
    }
}

fn chat_console_section(
    copy_id: SharedString,
    display: SharedString,
    copy_text: SharedString,
    command: bool,
    tokens: ThemeTokens,
) -> Div {
    let tip: SharedString = if command {
        "Copy command".into()
    } else {
        "Copy output".into()
    };
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(12.0))
        .whitespace_normal()
        .font_family(MONO_FONT)
        .text_size(px(13.0))
        .line_height(relative(1.6))
        .text_color(if command {
            tokens.text_primary
        } else {
            tokens.text_secondary
        })
        .child(div().flex_1().min_w_0().child(ToolPlainText {
            id: format!("{copy_id}-text").into(),
            text: display,
        }))
        .child(chat_copy_button(copy_id, copy_text, tip, tokens))
}

#[derive(Clone)]
struct ToolOutputView {
    full: SharedString,
    preview: SharedString,
    truncated: bool,
    show_full: bool,
}

fn chat_console_output(
    pane_id: usize,
    index: usize,
    command: ToolOutputView,
    output: ToolOutputView,
    failed: bool,
    tokens: ThemeTokens,
    on_show_more: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let has_command = !command.full.is_empty();
    let hidden = command.full.len().saturating_sub(command.preview.len())
        + output.full.len().saturating_sub(output.preview.len());
    let mut scroll = div()
        .id(SharedString::from(format!(
            "tool-console-{pane_id}-{index}"
        )))
        .w_full()
        .min_w_0()
        .max_h(px(240.0))
        .overflow_y_scroll();
    if has_command {
        scroll = scroll.child(chat_console_section(
            SharedString::from(format!("copy-tool-cmd-{pane_id}-{index}")),
            if command.show_full {
                command.full.clone()
            } else {
                command.preview.clone()
            },
            command.full,
            true,
            tokens,
        ));
    }
    if !output.full.is_empty() {
        scroll = scroll.child(
            div()
                .w_full()
                .min_w_0()
                .when(has_command, |section| {
                    section.border_t_1().border_color(tokens.border_subtle)
                })
                .bg(if failed {
                    tokens.block_failure.opacity(0.06)
                } else {
                    tokens.card_bg
                })
                .child(chat_console_section(
                    SharedString::from(format!("copy-tool-output-{pane_id}-{index}")),
                    if output.show_full {
                        output.full.clone()
                    } else {
                        output.preview.clone()
                    },
                    output.full,
                    false,
                    tokens,
                )),
        );
    }
    div()
        .w_full()
        .min_w_0()
        .rounded(px(6.0))
        .border_1()
        .border_color(if failed {
            tokens.block_failure.opacity(0.35)
        } else {
            tokens.border_subtle
        })
        .bg(tokens.window_bg)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(scroll)
        .when(command.truncated || output.truncated, |card| {
            card.child(chat_output_expander(
                SharedString::from(format!("tool-output-more-{pane_id}-{index}")),
                hidden,
                output.show_full,
                tokens,
                on_show_more,
            ))
        })
}

fn chat_tool_output_block(
    id: SharedString,
    text: SharedString,
    is_error: bool,
    tokens: ThemeTokens,
) -> Div {
    div()
        .w_full()
        .min_w_0()
        .rounded(px(6.0))
        .border_1()
        .border_color(if is_error {
            tokens.block_failure.opacity(0.35)
        } else {
            tokens.border_subtle
        })
        .bg(if is_error {
            tokens.block_failure.opacity(0.06)
        } else {
            tokens.card_bg
        })
        .px(px(12.0))
        .py(px(8.0))
        .whitespace_normal()
        .font_family(MONO_FONT)
        .text_size(px(13.0))
        .line_height(relative(1.6))
        .text_color(if is_error {
            tokens.block_failure
        } else {
            tokens.text_secondary
        })
        .child(
            div()
                .id(id.clone())
                .max_h(px(240.0))
                .overflow_y_scroll()
                .child(ToolPlainText {
                    id: format!("{id}-text").into(),
                    text,
                }),
        )
}

fn shell_command(input: &str, parsed: Option<&serde_json::Value>) -> String {
    parsed
        .and_then(|value| {
            ["command", "cmd"]
                .into_iter()
                .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
                .or_else(|| {
                    value
                        .get("metadata")
                        .and_then(|metadata| metadata.get("command"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            if parsed.is_some() || input.trim_start().starts_with('{') {
                String::new()
            } else {
                input.trim().to_owned()
            }
        })
}

fn tool_content_body(name: &str, parsed: Option<&serde_json::Value>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if !matches!(
        lower.as_str(),
        "write"
            | "write_file"
            | "create_file"
            | "edit"
            | "str_replace"
            | "str_replace_editor"
            | "patch"
            | "apply_patch"
    ) {
        return None;
    }
    let object = parsed?.as_object()?;
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
    };
    if let Some(content) = field(&["patch", "patchText", "patch_text", "diff"]) {
        return Some(content.to_owned());
    }
    if matches!(lower.as_str(), "write" | "write_file" | "create_file") {
        return field(&["content", "text"]).map(str::to_owned);
    }
    let mut diff = String::new();
    let decoded = object
        .get("edits")
        .and_then(serde_json::Value::as_str)
        .and_then(|edits| serde_json::from_str::<serde_json::Value>(edits).ok());
    if let Some(edits) = decoded
        .as_ref()
        .or_else(|| object.get("edits"))
        .and_then(serde_json::Value::as_array)
    {
        for edit in edits {
            append_tool_edit(&mut diff, edit);
        }
    } else {
        append_tool_edit(&mut diff, parsed?);
    }
    if diff.is_empty() {
        field(&["content", "newString", "new_string", "newText", "new_text"]).map(str::to_owned)
    } else {
        Some(diff)
    }
}

fn append_tool_edit(diff: &mut String, edit: &serde_json::Value) {
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| edit.get(*key).and_then(serde_json::Value::as_str))
    };
    let old = field(&["oldString", "old_string", "oldText", "old_text"]);
    let new = field(&["newString", "new_string", "newText", "new_text"]);
    let (Some(old), Some(new)) = (old, new) else {
        return;
    };
    diff.push_str("@@\n");
    for (prefix, text) in [('-', old), ('+', new)] {
        for line in text.split_inclusive('\n') {
            diff.push(prefix);
            diff.push_str(line);
            if !line.ends_with('\n') {
                diff.push('\n');
            }
        }
    }
}

fn style_auth_input(input: Input, tokens: ThemeTokens, focused: bool) -> Input {
    input
        .appearance(false)
        .bordered(false)
        .border_1()
        .rounded(px(6.0))
        .bg(tokens.pane_bg)
        .border_color(if focused {
            tokens.ring
        } else {
            tokens.card_border
        })
        .text_color(tokens.text_primary)
}

fn tool_paint_marker(id: Option<&str>, output: bool) -> Option<impl IntoElement> {
    let id = id
        .filter(|id| crate::stream_timing::tool_paint_due(id, output))?
        .to_owned();
    Some(
        canvas(
            |_, _, _| (),
            move |_, _, _, _| {
                if output {
                    crate::stream_timing::tool_output_painted(&id);
                } else {
                    crate::stream_timing::tool_card_painted(&id);
                }
            },
        )
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .size(px(1.0)),
    )
}

fn chat_tool_content(
    pane_id: usize,
    index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    on_show_more: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let mut body = div()
        .w_full()
        .min_w_0()
        .when(tool.display.shell_command.is_none(), |body| {
            body.pl(px(12.0))
        })
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(8.0));
    if !tool.output.is_empty()
        && let Some(marker) = tool_paint_marker(tool.id.as_deref(), true)
    {
        body = body.relative().child(marker);
    }

    if let Some(command) = tool.display.shell_command.as_ref() {
        body = body.child(chat_console_output(
            pane_id,
            index,
            ToolOutputView {
                full: command.clone(),
                preview: tool
                    .display
                    .shell_preview
                    .clone()
                    .unwrap_or_else(|| command.clone()),
                truncated: tool.display.shell_truncated,
                show_full: tool.show_full,
            },
            ToolOutputView {
                full: tool.output.clone(),
                preview: tool.preview.clone(),
                truncated: tool.preview_truncated,
                show_full: tool.show_full,
            },
            tool.state == ToolState::Failed,
            tokens,
            on_show_more,
        ));
    } else {
        let section_header = |label: &'static str, suffix: &'static str, text: SharedString| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(11.0))
                .text_color(tokens.text_muted)
                .child(label)
                .child(chat_copy_button(
                    format!("copy-tool-{suffix}-{pane_id}-{index}").into(),
                    text,
                    format!("Copy {}", label.to_ascii_lowercase()).into(),
                    tokens,
                ))
        };
        if tool.display.show_raw_input && !tool.display.input.is_empty() {
            let input = if tool.show_full {
                tool.display.input.clone()
            } else {
                tool.display.input_preview.clone()
            };
            body = body
                .child(section_header("Input", "input", tool.display.input.clone()))
                .child(chat_tool_output_block(
                    format!("tool-input-{pane_id}-{index}").into(),
                    input,
                    false,
                    tokens,
                ));
        }
        if let Some(content) = tool.display.content_body.as_ref() {
            let display = if tool.show_full {
                content.clone()
            } else {
                tool.display
                    .content_preview
                    .clone()
                    .unwrap_or_else(|| content.clone())
            };
            body = body
                .child(section_header(
                    if tool.display.view.title == "Write" {
                        "Content"
                    } else {
                        "Changes"
                    },
                    "content",
                    content.clone(),
                ))
                .child(chat_tool_output_block(
                    format!("tool-content-{pane_id}-{index}").into(),
                    display,
                    false,
                    tokens,
                ));
        }
        if !tool.output.trim().is_empty() {
            let display = if tool.show_full {
                tool.output.clone()
            } else {
                tool.preview.clone()
            };
            body = body.child(section_header("Output", "output", tool.output.clone()));
            body = body.child(chat_tool_output_block(
                format!("tool-output-{pane_id}-{index}").into(),
                display,
                tool.state == ToolState::Failed,
                tokens,
            ));
        }
        if (tool.display.show_raw_input && tool.display.input_truncated)
            || tool.display.content_truncated
            || tool.preview_truncated
        {
            let hidden = if tool.display.show_raw_input {
                tool.display
                    .input
                    .len()
                    .saturating_sub(tool.display.input_preview.len())
            } else {
                0
            } + tool.display.content_body.as_ref().map_or(0, |content| {
                content.len().saturating_sub(
                    tool.display
                        .content_preview
                        .as_ref()
                        .map_or(0, |preview| preview.len()),
                )
            }) + tool.output.len().saturating_sub(tool.preview.len());
            body = body.child(chat_output_expander(
                SharedString::from(format!("tool-body-more-{pane_id}-{index}")),
                hidden,
                tool.show_full,
                tokens,
                on_show_more,
            ));
        }
    }
    let notice = match tool.state {
        ToolState::Failed if tool.output.is_empty() => Some("Tool execution failed."),
        ToolState::Unknown if tool.output.is_empty() => Some("Execution ended without a result."),
        _ => None,
    };
    if let Some(notice) = notice {
        body = body.child(
            div()
                .text_size(px(12.0))
                .text_color(if tool.state == ToolState::Failed {
                    tokens.block_failure
                } else {
                    tokens.text_muted
                })
                .child(notice),
        );
    }
    body
}

enum FaviconAsset {}

impl gpui::Asset for FaviconAsset {
    type Source = String;
    type Output = Result<Arc<gpui::RenderImage>, gpui::ImageCacheError>;

    fn load(source: String, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        static SLOTS: std::sync::LazyLock<(
            smol::channel::Sender<()>,
            smol::channel::Receiver<()>,
        )> = std::sync::LazyLock::new(|| {
            let (sender, receiver) = smol::channel::bounded(4);
            for _ in 0..4 {
                let _ = sender.try_send(());
            }
            (sender, receiver)
        });
        let renderer = cx.svg_renderer();
        async move {
            let _ = SLOTS.1.recv().await;
            let result = smol::unblock(move || -> anyhow::Result<Arc<gpui::RenderImage>> {
                use std::io::Read as _;
                static CLIENT: std::sync::LazyLock<
                    Result<reqwest::blocking::Client, reqwest::Error>,
                > = std::sync::LazyLock::new(|| {
                    reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()
                });
                let client = CLIENT
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let response = client
                    .get("https://www.google.com/s2/favicons")
                    .query(&[("domain", source.as_str()), ("sz", "32")])
                    .send()?
                    .error_for_status()?;
                let mut bytes = Vec::new();
                response.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 64 * 1024, "favicon too large");
                gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes).to_image_data(renderer)
            })
            .await;
            let _ = SLOTS.0.try_send(());
            result.map_err(Into::into)
        }
    }
}

enum ProjectFaviconAsset {}

#[derive(Clone)]
struct LoadedProjectFavicon {
    image: Arc<gpui::RenderImage>,
    tone: ProjectFaviconTone,
}

#[derive(Clone, Copy)]
enum ProjectFaviconTone {
    Dark,
    Light,
    Mixed,
}

impl LoadedProjectFavicon {
    fn from_image(image: Arc<gpui::RenderImage>) -> Option<Self> {
        let mut opacity = 0usize;
        let mut dark = 0;
        let mut light = 0;
        let (pixels, _) = image.as_bytes(0)?.as_chunks::<4>();
        for &[b, g, r, a] in pixels {
            let alpha = usize::from(a);
            let brightness = (54 * u16::from(r) + 183 * u16::from(g) + 19 * u16::from(b)) / 256;
            opacity += alpha;
            if brightness <= 62 {
                dark += alpha;
            } else if brightness >= 193 {
                light += alpha;
            }
        }
        if opacity == 0 {
            return None;
        }
        let tone = if dark * 4 >= opacity * 3 {
            ProjectFaviconTone::Dark
        } else if light * 4 >= opacity * 3 {
            ProjectFaviconTone::Light
        } else {
            ProjectFaviconTone::Mixed
        };
        Some(Self { image, tone })
    }

    fn needs_backdrop(&self, dark_theme: bool) -> bool {
        matches!(
            (self.tone, dark_theme),
            (ProjectFaviconTone::Dark, true) | (ProjectFaviconTone::Light, false)
        )
    }
}

impl gpui::Asset for ProjectFaviconAsset {
    type Source = PathBuf;
    type Output = Result<LoadedProjectFavicon, gpui::ImageCacheError>;

    fn load(root: PathBuf, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        static SLOTS: std::sync::LazyLock<(
            smol::channel::Sender<()>,
            smol::channel::Receiver<()>,
        )> = std::sync::LazyLock::new(|| {
            let (sender, receiver) = smol::channel::bounded(4);
            for _ in 0..4 {
                let _ = sender.try_send(());
            }
            (sender, receiver)
        });
        let renderer = cx.svg_renderer();
        async move {
            let _ = SLOTS.1.recv().await;
            let result = smol::unblock(move || load_project_favicon(&root, &renderer)).await;
            let _ = SLOTS.0.try_send(());
            result.map_err(Into::into)
        }
    }
}

fn load_project_favicon(
    root: &Path,
    renderer: &gpui::SvgRenderer,
) -> anyhow::Result<LoadedProjectFavicon> {
    use std::io::Read as _;
    const MAX_BYTES: u64 = 1024 * 1024;
    const MAX_RASTER_SIDE: u32 = 2048;
    const ICON_PIXELS: u32 = 64;
    const CANDIDATES: &[&str] = &[
        "favicon.svg",
        "favicon.ico",
        "favicon.png",
        "favicon.webp",
        "public/favicon.svg",
        "public/favicon.ico",
        "public/favicon.png",
        "public/favicon.webp",
        "public/favicon/favicon.svg",
        "public/favicon/favicon.png",
        "public/favicon/favicon-96x96.webp",
        "app/favicon.ico",
        "app/icon.svg",
        "app/icon.png",
        "src/app/favicon.ico",
        "src/app/icon.svg",
        "src/app/icon.png",
        "static/favicon.svg",
        "static/favicon.ico",
        "static/favicon.png",
        "assets/favicon.svg",
        "assets/favicon.ico",
        "assets/favicon.png",
        "assets/favicon.webp",
        "assets/icon.svg",
        "assets/icon.png",
        "assets/logo.svg",
        "assets/logo.png",
        "assets/logo.webp",
        "web/favicon.png",
        "apps/web/public/favicon/favicon.svg",
        "apps/web/public/favicon.svg",
        "apps/web/public/favicon.ico",
        "apps/web/public/favicon.png",
        "apps/web/src/app/favicon.ico",
    ];
    for candidate in CANDIDATES {
        let path = root.join(candidate);
        if !path
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_BYTES)
        {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err()
            || bytes.len() as u64 > MAX_BYTES
        {
            continue;
        }
        if candidate.ends_with(".svg") {
            let side = gpui::DevicePixels(ICON_PIXELS as i32);
            if let Ok(svg) = renderer.parse_svg(&normalize_favicon_svg_colors(&bytes))
                && let Ok(image) =
                    renderer.render_parsed(&svg, gpui::SvgSize::Size(size(side, side)))
                && let Some(favicon) = LoadedProjectFavicon::from_image(image)
            {
                return Ok(favicon);
            }
            continue;
        }
        let Ok(mut reader) =
            image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()
        else {
            continue;
        };
        if !matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Ico | image::ImageFormat::WebP)
        ) {
            continue;
        }
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_RASTER_SIDE);
        limits.max_image_height = Some(MAX_RASTER_SIDE);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        let Ok(image) = reader.decode() else {
            continue;
        };
        let mut pixels = if image.width().max(image.height()) > ICON_PIXELS {
            image.thumbnail(ICON_PIXELS, ICON_PIXELS).into_rgba8()
        } else {
            image.into_rgba8()
        };
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image = Arc::new(gpui::RenderImage::new([image::Frame::new(pixels)]));
        if let Some(favicon) = LoadedProjectFavicon::from_image(image) {
            return Ok(favicon);
        }
    }
    anyhow::bail!("no project favicon")
}

fn normalize_favicon_svg_colors(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;

    static DISPLAY_P3: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\bcolor\(\s*display-p3\s+([^()]*)\)").unwrap()
    });
    let Ok(svg) = std::str::from_utf8(bytes) else {
        return Cow::Borrowed(bytes);
    };
    match DISPLAY_P3.replace_all(svg, |captures: &regex::Captures<'_>| {
        display_p3_hex(&captures[1]).unwrap_or_else(|| captures[0].to_owned())
    }) {
        Cow::Borrowed(svg) => Cow::Borrowed(svg.as_bytes()),
        Cow::Owned(svg) => Cow::Owned(svg.into_bytes()),
    }
}

fn display_p3_hex(value: &str) -> Option<String> {
    fn component(value: &str) -> Option<f64> {
        let value = value.trim();
        let number = match value.strip_suffix('%') {
            Some(percent) => f64::from(percent.parse::<f32>().ok()?) / 100.0,
            None => f64::from(value.parse::<f32>().ok()?),
        };
        number.is_finite().then_some(number)
    }
    fn linear(value: f64) -> f64 {
        if value.abs() <= 0.04045 {
            value / 12.92
        } else {
            ((value.abs() + 0.055) / 1.055).powf(2.4).copysign(value)
        }
    }
    fn encoded(value: f64) -> u8 {
        let value = value.clamp(0.0, 1.0);
        let value = if value <= 0.0031308 {
            12.92 * value
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        (value * 255.0).round() as u8
    }

    let (channels, alpha) = match value.split_once('/') {
        Some((channels, alpha)) => (channels, component(alpha)?),
        None => (value, 1.0),
    };
    let mut channels = channels.split_ascii_whitespace();
    let r = linear(component(channels.next()?)?);
    let g = linear(component(channels.next()?)?);
    let b = linear(component(channels.next()?)?);
    if channels.next().is_some() {
        return None;
    }

    // Linear Display P3 to linear sRGB, composed from CSS Color 4's D65 matrices.
    let red = encoded(1.224_940_18 * r - 0.224_940_18 * g);
    let green = encoded(-0.042_056_95 * r + 1.042_056_95 * g);
    let blue = encoded(-0.019_637_55 * r - 0.078_636_05 * g + 1.098_273_60 * b);
    let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(format!("#{red:02x}{green:02x}{blue:02x}{alpha:02x}"))
}

#[derive(IntoElement)]
struct ProjectFavicon {
    root: PathBuf,
    initials: SharedString,
    tokens: ThemeTokens,
    size: Pixels,
}

impl gpui::RenderOnce for ProjectFavicon {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};

        let frame = div()
            .size(self.size)
            .flex_none()
            .rounded(self.size * 0.24)
            .overflow_hidden();
        if let Some(Ok(favicon)) = window.use_asset::<ProjectFaviconAsset>(&self.root, cx) {
            return frame
                .when(favicon.needs_backdrop(self.tokens.is_dark), |frame| {
                    frame
                        .bg(self
                            .tokens
                            .pane_bg
                            .blend(self.tokens.text_primary.opacity(0.85)))
                        .p(self.size * 0.1)
                })
                .child(
                    img(favicon.image)
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                );
        }

        let mut hasher = DefaultHasher::new();
        self.root.hash(&mut hasher);
        let accent = Hsla {
            h: (hasher.finish() % 360) as f32 / 360.0,
            s: 0.72,
            l: if self.tokens.is_dark { 0.74 } else { 0.30 },
            a: 1.0,
        };
        frame
            .flex()
            .items_center()
            .justify_center()
            .bg(self.tokens.pane_bg.blend(accent.opacity(0.19)))
            .text_size(self.size * 0.49)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(accent)
            .child(self.initials)
    }
}

fn project_favicon(project: &Project, tokens: ThemeTokens, size: Pixels) -> ProjectFavicon {
    let mut initials: String = project
        .name
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .take(2)
        .collect();
    if initials.is_empty() {
        initials.push('?');
    }
    ProjectFavicon {
        root: project.path.clone(),
        initials: SharedString::from(initials),
        tokens,
        size,
    }
}

fn web_sources(name: &str, output: &str) -> Vec<String> {
    if !matches!(
        name.to_ascii_lowercase().as_str(),
        "websearch" | "web_search" | "search_web" | "webfetch" | "web_fetch" | "fetch"
    ) {
        return Vec::new();
    }
    let mut sources = Vec::new();
    // Keep work and retained data bounded even when a tool returns a large page.
    let end = output.floor_char_boundary(output.len().min(128 * 1024));
    for part in output[..end].split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | ',')
    }) {
        let Some(start) = part.find("https://").or_else(|| part.find("http://")) else {
            continue;
        };
        let candidate = part[start..].trim_end_matches(['.', ';']);
        if let Ok(url) = url::Url::parse(candidate)
            && let Some(host) = url.host_str()
        {
            let host = host.trim_start_matches("www.");
            if !sources.iter().any(|source| source == host) {
                sources.push(host.to_owned());
                if sources.len() == 100 {
                    break;
                }
            }
        }
    }
    sources
}

fn chat_tool_header(
    pane_id: usize,
    index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    use gpui_component::IconName;

    let view = &tool.display.view;
    let running = tool.state == ToolState::Running;
    let failed = tool.state == ToolState::Failed;
    let icon = match view.title.as_str() {
        "Batch" => IconName::GalleryVerticalEnd,
        "Shell" => IconName::SquareTerminal,
        "Read" => IconName::BookOpen,
        "List" => IconName::FolderClosed,
        "Web Search" | "Webfetch" | "Browser" => IconName::Globe,
        "Ripgrep" | "Search" | "Glob" => IconName::Search,
        "Edit" | "Patch" => IconName::Replace,
        _ if is_agent_tool(&tool.name) => IconName::Bot,
        _ => IconName::File,
    };

    let color = if failed {
        tokens.block_failure
    } else {
        tokens.text_primary
    };
    let icon_color = if failed { color } else { tokens.text_muted };

    let row = div()
        .id(SharedString::from(format!("tool-row-{pane_id}-{index}")))
        .debug_selector(move || format!("tool-row-{index}"))
        .w_full()
        .min_w_0()
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(13.0))
        .text_color(color)
        .cursor(CursorStyle::PointingHand)
        .hover(move |style| style.text_color(if failed { color } else { tokens.text_primary }))
        .on_mouse_down(MouseButton::Left, on_toggle)
        .child(
            div().flex_shrink_0().child(
                gpui_component::Icon::new(if failed {
                    IconName::TriangleAlert
                } else {
                    icon
                })
                .size(px(14.0))
                .text_color(icon_color),
            ),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(if failed {
                    format!("{} failed", view.title)
                } else {
                    view.title.clone()
                }),
        )
        .when(chat_batch::is_batch(&tool.name), |row| {
            row.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(tokens.text_muted)
                    .child(chat_batch::counts(tool)),
            )
        })
        .when_some(
            tool.batch_result
                .as_ref()
                .filter(|result| result.failed_count() > 0),
            |row, result| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_color(tokens.block_failure)
                        .child(format!("{} failed", result.failed_count())),
                )
            },
        )
        .when(running, |row| {
            row.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(tokens.text_muted)
                    .child("· Running…"),
            )
        })
        .when(
            !view.subtitle.is_empty() && !(view.title == "Shell" && tool.expanded),
            |row| {
                row.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(tokens.text_muted)
                        .child(view.subtitle.clone()),
                )
            },
        )
        .children(view.args.iter().map(|arg| {
            div()
                .min_w_0()
                .truncate()
                .text_color(tokens.text_muted)
                .child(arg.clone())
        }))
        .when(!tool.web_sources.is_empty(), |row| {
            let mut avatars = div().flex().items_center().flex_shrink_0();
            for (index, source) in tool.web_sources.iter().take(3).enumerate() {
                let initial = source
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_ascii_uppercase()
                    .to_string();
                let host = source.clone();
                let fallback_initial = initial.clone();
                let fallback = move || {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.0))
                        .text_color(tokens.text_primary)
                        .child(fallback_initial.clone())
                        .into_any_element()
                };
                let loading = fallback.clone();
                avatars = avatars.child(
                    div()
                        .when(index > 0, |avatar| avatar.ml(px(-4.0)))
                        .size(px(16.0))
                        .rounded_full()
                        .border_1()
                        .border_color(tokens.border_subtle)
                        .bg(tokens.sidebar_item_hover)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.0))
                        .text_color(tokens.text_primary)
                        .overflow_hidden()
                        .child(
                            img(move |window: &mut Window, cx: &mut App| {
                                window.use_asset::<FaviconAsset>(&host, cx)
                            })
                            .size_full()
                            .with_loading(loading)
                            .with_fallback(fallback),
                        ),
                );
            }
            row.child(avatars)
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(tokens.text_muted)
                        .child(format!(
                            "{} {}",
                            tool.web_sources.len(),
                            if tool.web_sources.len() == 1 {
                                "site"
                            } else {
                                "sites"
                            }
                        )),
                )
        })
        .child(
            svg()
                .path(if tool.expanded {
                    "icons/chevron-down.svg"
                } else {
                    "icons/chevron-right.svg"
                })
                .size(px(12.0))
                .flex_shrink_0()
                .text_color(tokens.text_muted),
        );

    row.into_any_element()
}
