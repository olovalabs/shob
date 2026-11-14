use super::{BladeContext, BladeRenderTarget, BladeRendererCore, wait_for_gpu};
use crate::{DevicePixels, PlatformAtlas, PlatformHeadlessRenderer, Scene, Size};
use anyhow::{Context as _, Result};
use blade_graphics as gpu;
use std::sync::Arc;

/// Vulkan's `optimalBufferCopyRowPitchAlignment` is a power of two no larger than
/// 256, so 256 satisfies it on every device. Rows copied out of a texture are
/// padded up to this, and the padding is stripped again when the image is built.
const COPY_BYTES_PER_ROW_ALIGNMENT: u32 = 256;

/// Renders GPUI scenes off-screen, with no window, surface, or display server.
///
/// The renderer owns a [`gpu::Context`] created with `presentation: false`, so it
/// needs a working Vulkan (or GL) driver but nothing from the window system --
/// which is what makes it usable from a test runner or CI.
///
/// Frames are drawn into a texture that is cached and reused while the size stays
/// the same. [`PlatformHeadlessRenderer::render_scene`] encodes, submits and
/// waits; [`PlatformHeadlessRenderer::render_scene_to_image`] additionally copies
/// the texture back to the CPU, so the readback cost is only paid when pixels are
/// actually wanted.
///
/// ## One frame at a time
///
/// Every frame waits for the previous one before the next is recorded, exactly as
/// the windowed renderer does. Letting frames overlap is faster but wrong: blade
/// syncs within a command buffer, not across submissions of the same reused
/// texture, and on radv a capture that follows an unwaited frame intermittently
/// reads back an empty target. The renderer therefore behaves like a window, not
/// like a pipelined game loop; benchmarks measure encoding and submission, which
/// is what a test can meaningfully measure anyway.
pub struct BladeHeadlessRenderer {
    core: BladeRendererCore,
    render_target: Option<BladeRenderTarget>,
}

impl BladeHeadlessRenderer {
    /// Creates a renderer on the best available device.
    ///
    /// With `allow_software_adapter` false, a software adapter is an error: it
    /// would silently turn every "GPU" measurement into a CPU rasterization
    /// benchmark. Tests that only want pixels rather than speed can opt in.
    pub fn new(allow_software_adapter: bool) -> Result<Self> {
        let context = BladeContext::new_headless()?;
        if !allow_software_adapter && context.gpu.device_information().is_software_emulated {
            anyhow::bail!(
                "the only available GPU adapter is a software rasterizer; \
                 pass `allow_software_adapter` if that is what you want"
            );
        }

        // The windowed path renders into a Vulkan swapchain image, which is
        // Bgra8Unorm with the sRGB encode done by the presentation pipeline. Using
        // the same format here means a headless frame and an on-screen frame hold
        // the same bytes, so the readback below is a plain channel swap.
        let surface_info = gpu::SurfaceInfo {
            format: gpu::TextureFormat::Bgra8Unorm,
            alpha: gpu::AlphaMode::Ignored,
        };
        let core = BladeRendererCore::new(&context, surface_info);

        Ok(Self {
            core,
            render_target: None,
        })
    }

    /// Allocates the render target if the size changed. Reusing it is what keeps
    /// repeated same-size frames from reallocating a texture every time.
    fn ensure_render_target(&mut self, size: Size<DevicePixels>) -> Result<()> {
        anyhow::ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "invalid headless render target size: {size:?}"
        );

        if let Some(target) = &self.render_target
            && target.size.width == size.width.0 as u32
            && target.size.height == size.height.0 as u32
        {
            return Ok(());
        }

        let extent = gpu::Extent {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth: 1,
        };
        let texture = self.core.gpu.create_texture(gpu::TextureDesc {
            name: "headless render target",
            format: self.core.target_format,
            size: extent,
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: gpu::TextureDimension::D2,
            // TARGET to draw into, COPY so it can be read back.
            usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
            external: None,
        });
        let view = self.core.gpu.create_texture_view(
            texture,
            gpu::TextureViewDesc {
                name: "headless render target view",
                format: self.core.target_format,
                dimension: gpu::ViewDimension::D2,
                subresources: &Default::default()
            },
        );

        if let Some(old) = self.render_target.take() {
            self.core.gpu.destroy_texture(old.texture);
            self.core.gpu.destroy_texture_view(old.view);
        }
        self.render_target = Some(BladeRenderTarget::offscreen(texture, view, extent));
        Ok(())
    }

    /// Records a frame into the render target. The command buffer is left open,
    /// so a caller that wants pixels can still record the readback copy into the
    /// same submission.
    fn record(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        self.ensure_render_target(size)?;
        let target = self
            .render_target
            .as_mut()
            .context("headless render target was not created")?;

        self.core.record_frame(
            scene,
            target,
            false,
            // Matches the windowed path, which clears to transparent black.
            gpu::TextureColor::TransparentBlack,
        );
        Ok(())
    }

    fn render(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        self.record(scene, size)?;
        let sync_point = self.core.submit();
        wait_for_gpu(&self.core.gpu, &sync_point);
        Ok(())
    }

    /// Submits the recorded frame, copies the render target back to the CPU, and
    /// returns it as RGBA8.
    ///
    /// The dimensions come from the target texture, so the result can never
    /// disagree with what was rendered.
    fn submit_and_read_image(&mut self) -> Result<image::RgbaImage> {
        let target = *self
            .render_target
            .as_ref()
            .context("headless render target was not created")?;
        let width = target.size.width;
        let height = target.size.height;
        let bytes_per_row = width
            .checked_mul(4)
            .context("headless render target row size overflowed")?;
        let padded_bytes_per_row = bytes_per_row
            .checked_next_multiple_of(COPY_BYTES_PER_ROW_ALIGNMENT)
            .context("headless padded row size overflowed")?;
        let buffer_size = u64::from(padded_bytes_per_row)
            .checked_mul(u64::from(height))
            .context("headless readback buffer size overflowed")?;

        // `Memory::Shared` is host-visible and, in blade, only ever coherent
        // memory types are allocated, so the buffer can be read directly once the
        // copy has completed.
        let readback_buffer = self.core.gpu.create_buffer(gpu::BufferDesc {
            name: "headless readback buffer",
            size: buffer_size,
            memory: gpu::Memory::Shared,
        });

        // The copy has to go into the frame's still-open command buffer: blade
        // ends that buffer on submit, and recording into an ended command buffer
        // is undefined behaviour.
        self.core
            .command_encoder
            .transfer("headless readback")
            .copy_texture_to_buffer(
                target.texture.into(),
                readback_buffer.into(),
                padded_bytes_per_row,
                gpu::Extent {
                    width,
                    height,
                    depth: 1,
                },
            );

        let sync_point = self.core.submit();
        wait_for_gpu(&self.core.gpu, &sync_point);

        let mapped = unsafe {
            std::slice::from_raw_parts(readback_buffer.data(), buffer_size as usize)
        };
        let mut pixels =
            Vec::with_capacity((bytes_per_row as usize).saturating_mul(height as usize));
        for row in mapped
            .chunks_exact(padded_bytes_per_row as usize)
            .take(height as usize)
        {
            // Drop the copy alignment padding at the end of each row.
            pixels.extend_from_slice(&row[..bytes_per_row as usize]);
        }

        // The target is Bgra8Unorm, matching a Vulkan swapchain image.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        self.core.gpu.destroy_buffer(readback_buffer);

        image::RgbaImage::from_raw(width, height, pixels)
            .context("failed to build an image from the headless pixel data")
    }
}

impl Drop for BladeHeadlessRenderer {
    fn drop(&mut self) {
        // Nothing can be in flight: every submit above waits for its own sync
        // point. Releasing the target needs no barrier, which also means this
        // destructor cannot panic -- it runs during unwinding whenever a test
        // fails, and a panic here would abort instead of failing the test.
        self.core.destroy();
        if let Some(target) = self.render_target.take() {
            self.core.gpu.destroy_texture(target.texture);
            self.core.gpu.destroy_texture_view(target.view);
        }
    }
}

impl PlatformHeadlessRenderer for BladeHeadlessRenderer {
    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        self.render(scene, size)
    }

    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<image::RgbaImage> {
        self.record(scene, size)?;
        self.submit_and_read_image()
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        Arc::clone(&self.core.atlas) as Arc<dyn PlatformAtlas>
    }
}
