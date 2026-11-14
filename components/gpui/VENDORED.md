# Vendored gpui 0.2.2

Copy of [`gpui`](https://crates.io/crates/gpui) 0.2.2 from crates.io with local
patches. It is wired in through `[patch.crates-io] gpui` in the workspace
`Cargo.toml`, so `app`, `gpui-component` and `terminal` all link this copy
instead of the registry crate.

## Why

gpui 0.2.2 cannot render a frame in a test. `TestWindow::draw` throws the scene
away:

```rust
fn draw(&self, _scene: &crate::Scene) {}
```

so `VisualTestContext` exercises state and input but never rasterizes anything:
no pixel assertions, no snapshots, no rendering benchmarks. On top of that
`TestPlatform` hardcodes `NoopTextSystem`, so text is never shaped either.

Upstream fixed this for its own `gpui_wgpu` backend in
zed-industries/zed#64718 ("Add Linux headless rendering"), but that lands in
Zed's in-tree `gpui`. 0.2.2 has no hooks to build on at all: `PlatformWindow` is
`pub(crate)`, there is no headless-renderer trait, and its Linux backend is
blade-graphics rather than wgpu — `gpui_wgpu` is not published.

The port follows the same shape as #64718: split the surface off the renderer,
keep a surface-less core that can encode a frame into any target, and wrap that
in a headless renderer the test platform routes window draws through.

## Patches on top of 0.2.2

### 1. `src/platform.rs`

- New `pub(crate) trait PlatformHeadlessRenderer`: `render_scene`,
  `render_scene_to_image`, `sprite_atlas`.
- New `pub(crate) fn platform_text_system()`, returning the platform's real text
  system (`CosmicTextSystem` on Linux, `MacTextSystem` on macOS) or an error where
  one cannot exist without a display (Windows builds its DirectWrite text system
  alongside the platform window).

The trait is crate-private on purpose, unlike #64718's public trait: there,
`gpui_platform` is a separate crate that has to name it. Here gpui owns the only
backend, and `Scene` and `PlatformTextSystem` are both `pub(crate)`, so making
the trait public would have forced a cascade of visibility changes across the
scene and text-system modules. Callers reach the feature through
`TestAppContext::with_headless_renderer` and `VisualTestContext::draw_frame`
instead, which is all a test ever needs.

### 2. `src/platform/blade/blade_renderer.rs`

- `BladeRenderer` split into `BladeRendererCore` (device, pipelines, atlas,
  instance belt, command encoder, frame encoding) plus the surface-owning
  `BladeRenderer`. Mirrors `WgpuRendererCore` in #64718.
- `BladeRendererCore::record_frame` takes a `BladeRenderTarget` and the
  premultiplied-alpha flag explicitly instead of reading them off a surface.
  `submit` is separate, so a caller can record more work into the same
  submission — which is exactly what the readback needs (see below).
- A target is built by `BladeRenderTarget::acquired` (a swapchain image, fresh
  from `acquire_frame` every frame) or `BladeRenderTarget::offscreen` (a texture
  the renderer keeps). The difference is `needs_init`: blade's `init_texture`
  transitions with `UNDEFINED` as the old layout and `TOP_OF_PIPE` as the source
  stage, which is fine for a swapchain image because nothing else can be reading
  it, but wrong for an image the previous frame may still be copying out of. A
  reused offscreen target is therefore transitioned exactly once, and afterwards
  left to the per-pass memory barriers. Re-declaring it every frame is at best
  redundant and at worst lets a frame start writing while the last one is still
  in flight.
- Path/MSAA intermediates are keyed by target size (`NO_INTERMEDIATES` sentinel)
  rather than by surface config, so they follow the render target.
- `wait_for_gpu` is a free function so both renderers share the hang diagnostics.

### 3. `src/platform/blade/blade_headless_renderer.rs` (new)

- Creates a `gpu::Context` with `presentation: false`, so blade never touches a
  surface, a window, or the display server. A working Vulkan driver is still
  required.
- Renders into an offscreen `Bgra8Unorm` texture, cached and reused while the size
  is unchanged. `Bgra8Unorm` is what a Vulkan swapchain image is on this backend
  (`surface.rs`), so a headless frame holds the same bytes an on-screen frame
  would, and readback is a channel swap rather than a colour conversion.
- `render_scene` only encodes and submits. `render_scene_to_image` additionally
  copies the texture to a `Memory::Shared` buffer and returns an
  `image::RgbaImage`: rows are padded to 256 bytes
  (`COPY_BYTES_PER_ROW_ALIGNMENT`, which satisfies every device's
  `optimalBufferCopyRowPitchAlignment`) and the padding is stripped, then BGRA is
  swapped to RGBA. blade only ever allocates host-visible *and* host-coherent
  memory (`init.rs`), so the buffer can be read directly after the wait — no
  flush or invalidate.
- The copy is recorded **before** `submit`. blade ends the command buffer inside
  `submit`, and recording into an ended command buffer crashes the driver; this
  was a real SIGSEGV before the record/submit split.
- Software adapters are rejected unless explicitly allowed, so a benchmark cannot
  silently measure CPU rasterization.
- `Drop` never waits while unwinding: blade's `wait_for` panics on a poisoned queue
  lock, which is exactly the state a lost device leaves behind, and a second panic
  during unwinding aborts the process instead of failing the test that caused it.

### 4. `src/platform/blade/blade_context.rs`

- `BladeContext::new_headless` (`presentation: false`), and the `ZED_DEVICE_ID`
  parsing shared by both paths via `device_id_filter`.

### 5. `src/platform/test/{platform,window}.rs`

- `TestPlatform` takes a text system and an optional headless-renderer factory.
  `open_window` builds the renderer and installs **its** atlas on the `TestWindow`,
  which matters: window painting resolves sprites through the renderer's atlas, so
  a window paired with a different atlas samples the wrong tiles.
- `TestWindow::draw` renders the scene at the window's device size (logical size
  times the test scale factor of 2). A capture flag makes one frame read its
  pixels back instead of discarding them, so a capture costs neither a scene clone
  nor a second render.
- `new_headless_renderer` is gated per platform: blade platforms get a real
  renderer, others get a clear error.

### 6. `src/app/test_context.rs`, `src/window.rs`

- `TestAppContext::with_headless_renderer` /
  `with_headless_renderer_allowing_software` build a context with a real text
  system and a renderer, returning `Result` so a machine with no usable GPU fails
  loudly instead of rendering nothing.
- `VisualTestContext::draw_frame` paints, presents, and returns the captured
  `RgbaImage`. Named `draw_frame` because `VisualTestContext::draw` already exists
  with a different meaning (drawing a single element for layout assertions).
- `add_sized_window_view` opens a window at an explicit size; the default
  maximized test window is 1920x1080, i.e. 3840x2160 device pixels to rasterize
  and read back.
- `Window::present` is now `pub(crate)` so the test context can drive
  presentation the way a platform's frame callback would.

### 7. `src/taffy.rs`

- `minmax(length(0.0), fr(1.0))` is written as `0.0_f32` / `1.0_f32`. Those helpers
  take `Into<f32>`, so an untyped literal falls back to f64, which does not
  implement `From<f64>` for f32. Upstream 0.2.2 predates
  `float_literal_f32_fallback` and warns on current rustc; it is a future hard
  error. This warning was invisible while gpui came from crates.io, because Cargo
  does not re-emit warnings for a cached dependency — vendoring made it visible.

### 8. `src/platform/linux/x11/window.rs`

- `rwh::HasWindowHandle for X11Window` and `rwh::HasDisplayHandle for X11Window`
  are implemented instead of `unimplemented!()`. They return the handle built from
  the state `X11WindowStatePtr` already holds, exactly as the `RawWindow` impl
  just above them does.

  This is not a cosmetic fix. `gpui::Window` forwards both trait methods to
  `self.platform_window`, which on X11 is an `X11Window`, so *any* caller of
  `window_handle()` on an X11 window — `app/src/linux_desktop.rs` setting
  `_NET_WM_ICON` is the first one we wrote — aborted the process with
  `not implemented` instead of returning a handle. The visual id is not kept
  next to the window id, so it is read back off the server; the screen index for
  the display handle is recovered by matching `x_root_window` against the setup's
  roots. `RawWindow` keeps its own impl because `BladeRenderer::new` still takes a
  `&RawWindow` before the `X11WindowState` exists.

### 9. Platform renderer compatibility and resource use

- `src/platform/windows/directx_devices.rs` retries D3D11 device creation with
  feature levels 11_0/10_1 if a runtime rejects a list containing 11_1. This
  preserves the hardware renderer on older D3D11 runtimes instead of aborting
  startup on feature-level negotiation.
- `src/platform/windows/directx_renderer.rs` allocates full-window path and
  4x-MSAA intermediates only when a scene has non-empty paths; it drops them on
  resize and recreates them lazily at the new size.
- `src/platform/windows/platform.rs` only broadcasts device-loss recovery after
  a replacement DirectX device was successfully created.
- `src/platform/mac/metal_renderer.rs` prefers built-in low-power devices over
  removable GPUs and defers full-window path/MSAA texture allocation until paths
  are actually drawn. Same-size path targets are reused.
- `src/platform/blade/blade_renderer.rs` likewise builds path intermediates only
  for non-empty path batches and releases them on target resize.

## Feature gating

The headless renderer and visual-test harness changes are behind
`#[cfg(any(test, feature = "test-support"))]`. The X11 window-handle fix and the
platform renderer fixes in sections 8-9 are runtime code and ship normally.
`app` enables `test-support` from a dev-dependency only; `components/gpui` is
listed in the workspace `exclude` so its own examples and tests are not pulled
in.

## Operating notes

**One frame at a time, by design.** Every headless frame waits for the previous
one before the next is recorded, the same way the windowed renderer does. This is
not a performance oversight: letting frames overlap is *wrong*. blade synchronizes
within a command buffer, not across submissions that reuse the same texture, and
the symptom on radv is a capture that intermittently reads back an empty
(transparent) frame — roughly one in thirty — plus, less often, a lost device:

```
radv/amdgpu: The CS has been cancelled because the context is lost.
This context is guilty of a hard recovery.
```

blade turns `ERROR_DEVICE_LOST` into `panic!("GPU has crashed")`, so that
surfaces as a crashed test rather than a failed assertion. Do not "optimize" the
wait away. It also means `VisualTestContext::render` measures encoding and
submission, serially — not GPU completion time and not pipelined throughput.

The tests in `app/tests/headless_render.rs` deliberately run in parallel with each
other (one GPU device each) and are stable, so no test-side serialization is
needed. An earlier version of this file blamed concurrent devices for the crashes;
it was the overlapping frames after all.

**A Vulkan driver is required.** Headless rendering here is still GPU rendering;
`presentation: false` means no surface and no display server, not no driver. On a
headless box install Mesa's lavapipe (`mesa-vulkan-drivers`) and use
`with_headless_renderer_allowing_software(true)`.

## Re-vendoring

Copy the new release over this directory, delete `examples/`, `tests/`, `docs/`
and `Cargo.lock`, strip the `[[example]]`, `[[test]]` and `[dev-dependencies]`
sections from its `Cargo.toml`, then re-apply all patches documented above. The
manifest header says the same thing where Cargo will show it.

**Keep `resources/`.** It is easy to miss because nothing on Linux or macOS
reads it, so a copy that looks complete on those platforms can still be missing
it. `build.rs` embeds `resources/windows/gpui.rc` and
`resources/windows/gpui.manifest.xml` into the binary on Windows, by path
relative to the crate root, and `.rc` in turn references the manifest by that
same relative path. Drop the directory and the Windows build dies with

```
fatal error RC1110: could not find resources/windows/gpui.rc
thread 'main' panicked at build.rs:275:14:
called `Result::unwrap()` on an `Err` value: Failed("RC.EXE failed to compile specified resource file")
```

which is what happened to the first 0.1.0 release attempt -- the directory was
never committed, so only CI, which builds all three platforms, ever noticed.

`src/platform/windows/shaders.hlsl` is needed too, for the same reason and with
no such error: `compile_shaders()` runs in release builds and reads it from
`CARGO_MANIFEST_DIR`, so a missing file is a silent no-op rather than a
failure. It is in `src/`, which is copied wholesale, so it cannot go missing
the way `resources/` did.

To check a re-vendor is complete, diff the file list against the published
crate rather than eyeballing it:

```sh
curl -sL -o gpui.crate https://static.crates.io/crates/gpui/gpui-0.2.2.crate
tar xzf gpui.crate
comm -23 <(cd gpui-0.2.2 && find . -type f | sed 's|^\./||' | grep -vE '^(docs|examples|tests)/' | sort) \
         <(cd components/gpui && find . -type f | sed 's|^\./||' | sort)
```

Anything listed there is either an intentional exclusion (`Cargo.lock`,
`Cargo.toml.orig`, `.cargo_vcs_info.json`) or a mistake.
