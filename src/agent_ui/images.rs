use std::{io::Read, sync::Arc};

use gpui::{
    AnyElement, App, IntoElement as _, ObjectFit, ParentElement as _, Styled as _,
    StyledImage as _, Window, div, img, px, relative,
};

use gpui::prelude::FluentBuilder;

use super::{UiCtx, prop_str};

const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGES: usize = 12;

enum CardImage {}

impl gpui::Asset for CardImage {
    type Source = String;
    type Output = Result<Arc<gpui::RenderImage>, gpui::ImageCacheError>;

    fn load(source: String, _: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        static SLOTS: std::sync::LazyLock<(
            smol::channel::Sender<()>,
            smol::channel::Receiver<()>,
        )> = std::sync::LazyLock::new(|| {
            let (tx, rx) = smol::channel::bounded(4);
            for _ in 0..4 {
                let _ = tx.try_send(());
            }
            (tx, rx)
        });
        async move {
            let _ = SLOTS.1.recv().await;
            let result = smol::unblock(move || load_image(source)).await;
            let _ = SLOTS.0.try_send(());
            result.map_err(Into::into)
        }
    }
}

fn load_image(source: String) -> anyhow::Result<Arc<gpui::RenderImage>> {
    static CLIENT: std::sync::LazyLock<Result<reqwest::blocking::Client, reqwest::Error>> =
        std::sync::LazyLock::new(|| {
            reqwest::blocking::Client::builder()
                .user_agent(concat!(
                    "Kathamo/",
                    env!("CARGO_PKG_VERSION"),
                    " (https://github.com/olovalabs/kathamo)"
                ))
                .timeout(std::time::Duration::from_secs(10))
                .build()
        });
    let input: Box<dyn Read> = if source.starts_with("https://") || source.starts_with("http://") {
        Box::new(
            CLIENT
                .as_ref()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .get(&source)
                .send()?
                .error_for_status()?,
        )
    } else {
        anyhow::ensure!(
            std::path::Path::new(&source).is_absolute(),
            "image path must be absolute"
        );
        Box::new(std::fs::File::open(source)?)
    };
    let mut bytes = Vec::new();
    input.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_IMAGE_BYTES, "image is too large");
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut pixels = reader.decode()?.into_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
        pixels,
    )])))
}

fn photo(source: String, alt: String, tokens: crate::theme::ThemeTokens) -> AnyElement {
    let fallback = move || {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(tokens.card_hover)
            .text_color(tokens.text_muted)
            .text_size(px(11.0))
            .child(format!("Image unavailable: {alt}"))
            .into_any_element()
    };
    div()
        .size_full()
        .overflow_hidden()
        .bg(tokens.card_hover)
        .child(
            img(move |window: &mut Window, cx: &mut App| {
                window.use_asset::<CardImage>(&source, cx)
            })
            .size_full()
            .object_fit(ObjectFit::Cover)
            .with_loading(move || div().size_full().bg(tokens.card_hover).into_any_element())
            .with_fallback(fallback),
        )
        .into_any_element()
}

pub(super) fn gallery(ctx: &UiCtx, props: &serde_json::Value) -> AnyElement {
    let images = props
        .get("images")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let images: Vec<_> = images
        .iter()
        .take(3)
        .filter_map(|item| {
            Some((
                prop_str(item, "src")?,
                prop_str(item, "alt").unwrap_or_else(|| "Image unavailable".into()),
            ))
        })
        .collect();
    if images.is_empty() {
        return super::error_chip(ctx.tokens, "Gallery needs images".into());
    }
    let mut outer = div()
        .w_full()
        .h(px(300.0))
        .flex()
        .gap(px(2.0))
        .rounded(px(12.0))
        .overflow_hidden();
    let (src, alt) = &images[0];
    let main = div()
        .h_full()
        .min_w_0()
        .when(images.len() > 1, |d| d.w(relative(0.6)))
        .when(images.len() == 1, |d| d.w_full())
        .child(photo(src.clone(), alt.clone(), ctx.tokens));
    outer = outer.child(main);
    if images.len() > 1 {
        let mut side = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (src, alt) in images.iter().skip(1) {
            side = side.child(div().flex_1().min_h_0().child(photo(
                src.clone(),
                alt.clone(),
                ctx.tokens,
            )));
        }
        outer = outer.child(side);
    }
    outer.into_any_element()
}

pub(super) fn image_list(ctx: &UiCtx, props: &serde_json::Value) -> AnyElement {
    let items = props
        .get("items")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut list = div().w_full().flex().flex_col();
    for (index, item) in items.iter().take(MAX_IMAGES).enumerate() {
        let Some(src) = prop_str(item, "src") else {
            continue;
        };
        let title = prop_str(item, "title").unwrap_or_default();
        let description = prop_str(item, "description").unwrap_or_default();
        let alt = prop_str(item, "alt").unwrap_or_else(|| title.clone());
        list = list.child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap(px(12.0))
                .py(px(12.0))
                .when(index > 0, |d| {
                    d.border_t_1().border_color(ctx.tokens.border_subtle)
                })
                .child(
                    div()
                        .size(px(96.0))
                        .flex_shrink_0()
                        .rounded(px(10.0))
                        .overflow_hidden()
                        .child(photo(src, alt, ctx.tokens)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .text_color(ctx.tokens.text_primary)
                        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                        .child(
                            div()
                                .text_color(ctx.tokens.text_secondary)
                                .child(description),
                        ),
                ),
        );
    }
    list.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn loads_remote_images_from_servers_requiring_a_user_agent() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("user-agent: kathamo/")
            );
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                1,
                1,
                image::Rgba([10, 20, 30, 255]),
            ))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
            let png = png.into_inner();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                png.len()
            )
            .unwrap();
            stream.write_all(&png).unwrap();
        });
        let image = load_image(format!("http://{address}/logo.png")).unwrap();
        assert_eq!(image.as_bytes(0).unwrap(), &[30, 20, 10, 255]);
        server.join().unwrap();
    }
}
