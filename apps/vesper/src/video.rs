use eframe::{
    egui,
    glow::{self, HasContext},
};
use std::{
    ffi::{CStr, c_char, c_void},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use vesper::mpv::{GlFbo, GlInit, Handle, Param, Player};

pub type Proc = Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;
struct Callbacks {
    ctx: egui::Context,
    proc: Proc,
    wakeups: AtomicUsize,
}
unsafe extern "C" fn resolve(data: *mut c_void, name: *const c_char) -> *mut c_void {
    // mpv invokes this only during context creation. The pinned box outlives the render context.
    let data = unsafe { &*data.cast::<Callbacks>() };
    (data.proc)(unsafe { CStr::from_ptr(name) }).cast_mut()
}
unsafe extern "C" fn wake(data: *mut c_void) {
    if !data.is_null() {
        let data = unsafe { &*data.cast::<Callbacks>() };
        data.wakeups.fetch_add(1, Ordering::Relaxed);
        data.ctx.request_repaint();
    }
}

pub struct Video {
    pub player: Arc<Player>,
    handle: Handle,
    callbacks: Box<Callbacks>,
    fbo: Option<glow::Framebuffer>,
    texture: Option<glow::Texture>,
    size: [i32; 2],
    pub frames: usize,
    /// Renders the decoder asked for (a new picture or overlay), not resize redraws.
    pub new_frames: usize,
    pub error: Option<String>,
    pub capture: Option<std::path::PathBuf>,
}
// The paint callback is Send+Sync by egui contract. All rendering and destruction occur
// on eframe's UI/GL thread, protected by one mutex; mpv wake callbacks never acquire it.
unsafe impl Send for Video {}
impl Video {
    pub fn new(
        ctx: egui::Context,
        proc: Option<Proc>,
        player: Arc<Player>,
    ) -> Result<Arc<Mutex<Self>>, String> {
        let proc = proc.ok_or("OpenGL is unavailable.")?;
        let mut callbacks = Box::new(Callbacks {
            ctx,
            proc,
            wakeups: AtomicUsize::new(0),
        });
        let data = (&mut *callbacks as *mut Callbacks).cast();
        let mut init = GlInit {
            get_proc_address: resolve,
            context: data,
        };
        let mut params = [
            Param {
                kind: 1,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            Param::new(2, &mut init),
            Param::end(),
        ];
        let mut handle = ptr::null_mut();
        player.api.check(unsafe {
            (player.api.render_create)(&mut handle, player.handle, params.as_mut_ptr())
        })?;
        unsafe {
            (player.api.render_callback)(handle, Some(wake), data);
            (player.api.wakeup)(player.handle, Some(wake), data);
        }
        Ok(Arc::new(Mutex::new(Self {
            player,
            handle,
            callbacks,
            fbo: None,
            texture: None,
            size: [0, 0],
            frames: 0,
            new_frames: 0,
            error: None,
            capture: None,
        })))
    }
    pub fn callback(this: Arc<Mutex<Self>>, rect: egui::Rect) -> egui::PaintCallback {
        egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                if let Ok(mut v) = this.lock()
                    && let Err(e) = unsafe { v.paint(info, painter) }
                {
                    v.error = Some(e);
                }
            })),
        }
    }
    pub fn wakeups(&self) -> usize {
        self.callbacks.wakeups.load(Ordering::Relaxed)
    }
    /// A hash of every pixel currently in the video framebuffer (benchmark proof that the decoded
    /// picture changes). Reads the framebuffer back, so it is used at a few fixed moments only.
    pub fn frame_hash(&mut self, gl: &glow::Context) -> Option<u64> {
        let fbo = self.fbo?;
        let [width, height] = self.size;
        if width <= 0 || height <= 0 {
            return None;
        }
        let mut rgba = vec![0_u8; (width * height * 4) as usize];
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.read_pixels(
                0,
                0,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rgba)),
            );
        }
        Some(rgba.iter().fold(0xcbf29ce484222325_u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        }))
    }
    /// Auto-ending harness: same native video FBO, no desktop input or UI paint.
    pub fn hidden_render(&mut self, painter: &egui_glow::Painter) -> Result<(), String> {
        // Reuse the UI's current target size. Forcing 320x180 while the same
        // surface paints the UI would create artificial resize/render churn.
        let size = if self.size[0] > 0 {
            self.size
        } else {
            [320, 180]
        };
        let info = egui::PaintCallbackInfo {
            viewport: egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0] as f32, size[1] as f32),
            ),
            clip_rect: egui::Rect::EVERYTHING,
            pixels_per_point: 1.0,
            screen_size_px: [size[0] as u32, size[1] as u32],
        };
        unsafe { self.paint(info, painter) }
    }
    unsafe fn paint(
        &mut self,
        info: egui::PaintCallbackInfo,
        painter: &egui_glow::Painter,
    ) -> Result<(), String> {
        let gl = painter.gl();
        let viewport = info.viewport_in_pixels();
        let width = viewport.width_px.clamp(1, 3840);
        let height = viewport.height_px.clamp(1, 2160);
        unsafe {
            if self.fbo.is_none() {
                self.fbo = Some(gl.create_framebuffer()?);
                self.texture = Some(gl.create_texture()?);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.fbo);
            let resized = self.size != [width, height];
            if resized {
                gl.bind_texture(glow::TEXTURE_2D, self.texture);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    width,
                    height,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    self.texture,
                    0,
                );
                if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                    return Err("Video framebuffer is incomplete.".into());
                }
                self.size = [width, height];
            }
            gl.disable(glow::SCISSOR_TEST);
            let mut fbo = GlFbo {
                fbo: self.fbo.unwrap().0.get() as i32,
                width,
                height,
                internal_format: glow::RGBA8 as i32,
            };
            let mut flip = 1;
            let mut block = 0;
            let mut params = [
                Param::new(3, &mut fbo),
                Param::new(4, &mut flip),
                Param::new(12, &mut block),
                Param::end(),
            ];
            let new_frame = (self.player.api.render_update)(self.handle) & 1 != 0;
            let render_needed = new_frame || resized;
            if render_needed {
                self.player
                    .api
                    .check((self.player.api.render)(self.handle, params.as_mut_ptr()))?;
                self.frames += 1;
                if new_frame {
                    self.new_frames += 1;
                }
            }
            if let Some(path) = self.capture.take() {
                let mut rgba = vec![0_u8; (width * height * 4) as usize];
                gl.bind_framebuffer(glow::FRAMEBUFFER, self.fbo);
                gl.read_pixels(
                    0,
                    0,
                    width,
                    height,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut rgba)),
                );
                let mut bgr = vec![0; rgba.len()];
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let from = ((height as usize - 1 - y) * width as usize + x) * 4;
                        let to = (y * width as usize + x) * 4;
                        bgr[to..to + 4].copy_from_slice(&[
                            rgba[from + 2],
                            rgba[from + 1],
                            rgba[from],
                            255,
                        ]);
                    }
                }
                vesper::smoke::write_png(&path, &bgr, width as u32, height as u32)?;
            }
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, self.fbo);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, painter.intermediate_fbo());
            let x = viewport.left_px;
            let y = viewport.from_bottom_px;
            gl.blit_framebuffer(
                0,
                0,
                width,
                height,
                x,
                y,
                x + viewport.width_px,
                y + viewport.height_px,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, painter.intermediate_fbo());
            gl.enable(glow::SCISSOR_TEST);
            if render_needed {
                (self.player.api.report_swap)(self.handle);
            }
        }
        Ok(())
    }
    pub fn destroy(&mut self, gl: &glow::Context) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            (self.player.api.wakeup)(self.player.handle, None, ptr::null_mut());
            (self.player.api.render_callback)(self.handle, None, ptr::null_mut());
            (self.player.api.render_free)(self.handle);
            if let Some(f) = self.fbo.take() {
                gl.delete_framebuffer(f);
            }
            if let Some(t) = self.texture.take() {
                gl.delete_texture(t);
            }
        }
        self.handle = ptr::null_mut();
    }
}
impl Drop for Video {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.player.api.wakeup)(self.player.handle, None, ptr::null_mut());
                (self.player.api.render_callback)(self.handle, None, ptr::null_mut());
                (self.player.api.render_free)(self.handle);
            }
        }
        let _ = &self.callbacks;
    }
}
