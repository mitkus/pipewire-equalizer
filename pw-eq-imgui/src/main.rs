use std::{num::NonZeroU32, sync::Arc};

mod autoeq;
mod filter;
mod pipewire;
mod save_load;
mod state;
mod tray;

use dear_imgui_glow::GlowRenderer;
use dear_imgui_rs::*;
use dear_imgui_winit::{HiDpiMode, WinitPlatform};
use dear_implot::{ImPlotExt, PlotContext};
use glow::HasContext;
use glutin::{
    config::{Config as GlConfig, ConfigTemplateBuilder},
    context::{
        ContextAttributesBuilder, NotCurrentContext, NotCurrentGlContext, PossiblyCurrentContext,
        PossiblyCurrentGlContext,
    },
    display::{GetGlDisplay, GlDisplay},
    surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface},
};
use ksni::TrayMethods;
use pw_eq::tui::Notif;
use pw_util::NodeInfo;
use raw_window_handle::HasWindowHandle;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    platform::{wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11},
    window::{Window, WindowAttributes, WindowId},
};

use pipewire::PipewireState;
use state::ImguiState;
use tray::{Tray, TrayEvent};

/// Events injected into the winit loop from other threads. Each one wakes the (otherwise
/// idle, `ControlFlow::Wait`) event loop.
#[derive(Debug)]
pub enum UserEvent {
    Notif(Notif),
    Tray(TrayEvent),
}

/// Extra frames rendered after an event so ImGui can settle hover/focus/auto-resize state.
const SETTLE_FRAMES: u8 = 2;
/// Frames are seconds apart when idle; don't feed ImGui huge delta times.
const MAX_DELTA_TIME: f32 = 0.1;

/// Actions requested from inside the ImGui frame (menu items, shortcuts).
#[derive(Debug, Clone, Copy)]
enum Action {
    HideWindow,
    Quit,
}

/// Everything tied to a live OS window. On Wayland a window can't be hidden, so "minimize to
/// tray" destroys this and "show" recreates it; everything else in [`App`] survives.
struct Gfx {
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    window: Arc<Window>,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    default_audio_sink: Option<NodeInfo>,
    start_hidden: bool,
    tray: Option<ksni::Handle<Tray>>,
    /// Last (window_shown, bypass, eq_name) pushed to the tray.
    tray_state: Option<(bool, bool, String)>,

    gl_config: Option<GlConfig>,
    not_current: Option<NotCurrentContext>,
    gfx: Option<Gfx>,
    imgui: Option<ImguiState>,
    pipewire: Option<PipewireState>,

    settle_frames: u8,
    pending_action: Option<Action>,
}

fn window_attributes(visible: bool) -> WindowAttributes {
    // The general/instance name becomes the Wayland app_id / X11 WM_CLASS, which is how the
    // compositor finds pw-eq-imgui.desktop (window icon, taskbar grouping).
    let attrs = Window::default_attributes()
        .with_title(tray::APP_TITLE)
        .with_inner_size(LogicalSize::new(900.0, 900.0))
        // Only matters on X11 (Wayland ignores it): a --hidden start must never map the window.
        .with_visible(visible);
    let attrs = WindowAttributesExtWayland::with_name(attrs, tray::APP_ID, tray::APP_ID);
    WindowAttributesExtX11::with_name(attrs, tray::APP_ID, tray::APP_ID)
}

fn create_surface(
    cfg: &GlConfig,
    window: &Window,
) -> Result<Surface<WindowSurface>, Box<dyn std::error::Error>> {
    // Request an sRGB-capable framebuffer for consistent visuals
    let size = window.inner_size();
    let surface_attribs = SurfaceAttributesBuilder::<WindowSurface>::new()
        .with_srgb(Some(true))
        .build(
            window.window_handle()?.as_raw(),
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
    Ok(unsafe { cfg.display().create_window_surface(cfg, &surface_attribs)? })
}

fn dpi_mode(window: &Window) -> HiDpiMode {
    let scale_factor = window.scale_factor();
    if scale_factor != 1.0 {
        HiDpiMode::Locked(scale_factor)
    } else {
        HiDpiMode::Default
    }
}

impl App {
    fn window_shown(&self) -> bool {
        self.gfx.is_some()
    }

    /// First-time setup: window, GL context, ImGui, PipeWire thread.
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Box<dyn std::error::Error>> {
        let (window, cfg) = glutin_winit::DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes(!self.start_hidden)))
            .build(event_loop, ConfigTemplateBuilder::new(), |mut configs| {
                configs.next().unwrap()
            })?;
        let window = Arc::new(window.expect("DisplayBuilder did not create a window"));

        let context_attribs =
            ContextAttributesBuilder::new().build(Some(window.window_handle()?.as_raw()));
        let context = unsafe { cfg.display().create_context(&cfg, &context_attribs)? };

        let surface = create_surface(&cfg, &window)?;
        let context = context.make_current(&surface)?;
        // winit paces redraws with compositor frame callbacks (see pre_present_notify), so the
        // swap itself must never block the UI thread.
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            tracing::warn!(error = %err, "failed to set swap interval");
        }

        // Setup Dear ImGui
        let mut imgui_context = Context::create();
        imgui_context.set_ini_filename(None::<String>).unwrap();

        let io = imgui_context.io_mut();
        io.set_config_dpi_scale_fonts(true);
        // A blinking caret would need periodic redraws while idle.
        io.set_config_input_text_cursor_blink(false);

        const FONT_DATA: &[u8] = include_bytes!("../ProggyVector.ttf");
        imgui_context.fonts().add_font(&[FontSource::TtfData {
            data: FONT_DATA,
            size_pixels: Some(13.0),
            config: None,
        }]);

        let mut platform = WinitPlatform::new(&mut imgui_context);
        platform.attach_window(&window, dpi_mode(&window), &mut imgui_context);

        // Create Glow context and renderer
        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|s| {
                context.display().get_proc_address(s).cast()
            })
        };

        let mut renderer = GlowRenderer::new(gl, &mut imgui_context)?;
        // Use sRGB framebuffer: enable FRAMEBUFFER_SRGB during ImGui rendering
        renderer.set_framebuffer_srgb_enabled(true);
        renderer.new_frame()?;

        let pipewire = PipewireState::new(self.default_audio_sink.take(), self.proxy.clone());

        let mut imgui = ImguiState {
            plot_context: PlotContext::create(&imgui_context),
            context: imgui_context,
            platform,
            renderer,
            clear_color: [0.1, 0.2, 0.3, 1.0],
            auto_eq: autoeq::AutoEqWindowState::new(pipewire.notifs_tx.clone()),
            filter: filter::FilterWindowState::new(pipewire.sample_rate),
            save_load: save_load::SaveLoadWindowState::new(),
        };

        // Restore the last EQ so audio is processed even when started hidden.
        if let Some(conf) = imgui.save_load.load_last() {
            let name = imgui.save_load.path_filename().unwrap_or("config").to_string();
            imgui.filter.set_eq_apo(name, conf);
        }

        self.gl_config = Some(cfg);
        self.gfx = Some(Gfx {
            context,
            surface,
            window,
        });
        self.imgui = Some(imgui);
        self.pipewire = Some(pipewire);
        self.sync_pipewire();
        Ok(())
    }

    /// Recreates the OS window after [`Self::hide_window`].
    fn show_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        if let Err(err) = self.try_show_window(event_loop) {
            tracing::error!(error = %err, "failed to recreate window");
        }
        self.sync_tray();
    }

    fn try_show_window(
        &mut self,
        event_loop: &ActiveEventLoop,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = self.gl_config.as_ref().ok_or("not initialised")?;
        let not_current = self.not_current.take().ok_or("GL context missing")?;
        let imgui = self.imgui.as_mut().ok_or("ImGui state missing")?;

        let window = Arc::new(glutin_winit::finalize_window(
            event_loop,
            window_attributes(true),
            cfg,
        )?);
        let surface = create_surface(cfg, &window)?;
        let context = match not_current.make_current(&surface) {
            Ok(context) => context,
            Err(err) => {
                // Keep the context; without it the renderer's GL objects are gone for good.
                tracing::error!(error = %err, "make_current failed");
                return Err(err.into());
            }
        };
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            tracing::warn!(error = %err, "failed to set swap interval");
        }

        imgui
            .platform
            .attach_window(&window, dpi_mode(&window), &mut imgui.context);

        self.gfx = Some(Gfx {
            context,
            surface,
            window,
        });
        self.request_redraw(SETTLE_FRAMES + 1);
        Ok(())
    }

    /// Destroys the OS window (Wayland has no hide) but keeps GL context, ImGui and PipeWire.
    fn hide_window(&mut self) {
        let Some(Gfx {
            context,
            surface,
            window,
        }) = self.gfx.take()
        else {
            return;
        };
        // The window dies before its key-release / focus-loss events arrive, so tell ImGui
        // ourselves; a focus loss clears all held keys and modifiers.
        if let Some(imgui) = self.imgui.as_mut() {
            imgui
                .platform
                .handle_window_event(&mut imgui.context, &window, &WindowEvent::Focused(false));
        }
        match context.make_not_current() {
            Ok(not_current) => self.not_current = Some(not_current),
            Err(err) => tracing::error!(error = %err, "make_not_current failed"),
        }
        // Surface must go before the window it was created for.
        drop(surface);
        drop(window);
        self.settle_frames = 0;
        self.sync_tray();
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(tray) = self.tray.take() {
            futures_executor::block_on(tray.shutdown());
        }
        self.hide_window();
        if let Some(pipewire) = self.pipewire.as_mut() {
            pipewire.close();
        }
        event_loop.exit();
    }

    fn request_redraw(&mut self, settle_frames: u8) {
        if let Some(gfx) = &self.gfx {
            self.settle_frames = self.settle_frames.max(settle_frames);
            gfx.window.request_redraw();
        }
    }

    /// Pushes pending EQ changes to PipeWire. Independent of the window so it also works
    /// when hidden in the tray. No-ops when nothing changed.
    fn sync_pipewire(&mut self) {
        let (Some(imgui), Some(pipewire)) = (self.imgui.as_mut(), self.pipewire.as_mut()) else {
            return;
        };
        if imgui.filter.need_module_load() {
            pipewire.load_module(&mut imgui.filter);
        }
        if let Some(node_id) = pipewire.active_node_id {
            // Ok to call these often because they no-op if no updates needed
            imgui.filter.apply_all_to_pipewire(node_id);
            imgui.filter.apply_preamp_to_pipewire(node_id);
        }
    }

    /// Mirrors window/bypass/EQ-name state into the tray icon (tooltip + menu).
    fn sync_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let Some(imgui) = &self.imgui else { return };
        let shown = self.window_shown();
        let bypass = imgui.filter.bypass();
        let name = imgui.filter.eq.name.clone();
        let state = (shown, bypass, name.clone());
        if self.tray_state.as_ref() == Some(&state) {
            return;
        }
        self.tray_state = Some(state);
        let tray = tray.clone();
        tokio::spawn(async move {
            tray.update(move |t: &mut Tray| {
                t.window_shown = shown;
                t.bypass = bypass;
                t.eq_name = name;
            })
            .await;
        });
    }

    fn handle_tray_event(&mut self, event_loop: &ActiveEventLoop, event: TrayEvent) {
        match event {
            TrayEvent::ToggleWindow => {
                if self.window_shown() {
                    self.hide_window();
                } else {
                    self.show_window(event_loop);
                }
            }
            TrayEvent::ToggleBypass => {
                if let Some(imgui) = self.imgui.as_mut() {
                    let bypass = !imgui.filter.bypass();
                    imgui.filter.set_bypass(bypass);
                }
                self.sync_pipewire();
                self.sync_tray();
                self.request_redraw(SETTLE_FRAMES);
            }
            TrayEvent::Quit => self.quit(event_loop),
        }
    }

    fn handle_action(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::HideWindow if self.tray.is_some() => self.hide_window(),
            Action::HideWindow | Action::Quit => self.quit(event_loop),
        }
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        let Some(gfx) = &self.gfx else { return };
        if new_size.width > 0 && new_size.height > 0 {
            gfx.surface.resize(
                &gfx.context,
                NonZeroU32::new(new_size.width).unwrap(),
                NonZeroU32::new(new_size.height).unwrap(),
            );
        }
    }

    fn render(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (Some(gfx), Some(imgui), Some(pipewire)) =
            (self.gfx.as_ref(), self.imgui.as_mut(), self.pipewire.as_mut())
        else {
            return Ok(());
        };
        let tray_available = self.tray.is_some();

        // prepare_frame writes the real elapsed time into io.delta_time; cap it afterwards so a
        // long idle/hidden stretch doesn't advance ImGui's timers (key repeat etc.) in one jump.
        imgui.platform.prepare_frame(&gfx.window, &mut imgui.context);
        let io = imgui.context.io_mut();
        let delta_time = io.delta_time().min(MAX_DELTA_TIME);
        io.set_delta_time(delta_time);

        let ui = imgui.context.frame();

        // Menu
        {
            let _menu_tok = ui.begin_main_menu_bar();
            ui.menu("File", || {
                if ui.menu_item("Save/Load...") {
                    imgui.save_load.show_window = true;
                }
                ui.separator();
                if tray_available && ui.menu_item_with_shortcut("Hide to tray", "Ctrl+H") {
                    self.pending_action = Some(Action::HideWindow);
                }
                if ui.menu_item_with_shortcut("Quit", "Ctrl+Q") {
                    self.pending_action = Some(Action::Quit);
                }
            });
            ui.menu("Windows", || {
                ui.menu_item_toggle("AutoEQ", Some("Ctrl+A"), &mut imgui.auto_eq.show_window, true);
                ui.menu_item_toggle("Filter", Some("Ctrl+F"), &mut imgui.filter.show_window, true);
            });
        }

        // Global shortcuts
        if ui.io().key_ctrl() {
            if ui.is_key_pressed(input::Key::A) {
                imgui.auto_eq.show_window = !imgui.auto_eq.show_window;
            }
            if ui.is_key_pressed(input::Key::F) {
                imgui.filter.show_window = !imgui.filter.show_window;
            }
            if tray_available && ui.is_key_pressed(input::Key::H) {
                self.pending_action = Some(Action::HideWindow);
            }
            if ui.is_key_pressed(input::Key::Q) {
                self.pending_action = Some(Action::Quit);
            }
        }

        // AutoEq window
        if imgui.auto_eq.show_window {
            imgui.auto_eq.draw_window(ui, pipewire.sample_rate);
            if let Some((name, eq)) = imgui.auto_eq.get_eq_to_set() {
                imgui.filter.set_eq(name, eq);
            }
        }

        // Filter window
        if imgui.filter.show_window {
            let plot_ui = ui.implot(&imgui.plot_context);
            imgui.filter.draw_window(ui, &plot_ui, pipewire.sample_rate);
        }

        // Save/Load window
        if imgui.save_load.show_window {
            imgui.save_load.draw_window(ui, &imgui.filter.eq);
            if let Some(conf) = imgui.save_load.loaded_conf() {
                let name = imgui.save_load.path_filename().unwrap();
                imgui.filter.set_eq_apo(name, conf);
            }
        }

        // Render
        let gl = imgui.renderer.gl_context().unwrap();
        unsafe {
            // Enable sRGB write for clear on sRGB-capable surface
            gl.enable(glow::FRAMEBUFFER_SRGB);
            gl.clear_color(
                imgui.clear_color[0],
                imgui.clear_color[1],
                imgui.clear_color[2],
                imgui.clear_color[3],
            );
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.disable(glow::FRAMEBUFFER_SRGB);
        }

        imgui.platform.prepare_render_with_ui(ui, &gfx.window);
        let draw_data = imgui.context.render();

        imgui.renderer.new_frame()?;
        imgui.renderer.render(draw_data)?;

        // Lets winit throttle the next RedrawRequested to the compositor's frame callback.
        gfx.window.pre_present_notify();
        gfx.surface.swap_buffers(&gfx.context)?;

        Ok(())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.imgui.is_some() {
            return;
        }
        if let Err(e) = self.init(event_loop) {
            tracing::error!(error = %e, "failed to create window");
            event_loop.exit();
            return;
        }
        if self.start_hidden {
            self.hide_window();
        } else {
            self.request_redraw(SETTLE_FRAMES);
            self.sync_tray();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Notif(notif) => {
                if let (Some(imgui), Some(pipewire)) = (self.imgui.as_mut(), self.pipewire.as_mut())
                {
                    pipewire.handle_notif(notif, &mut imgui.filter, &mut imgui.auto_eq);
                }
                self.sync_pipewire();
                self.request_redraw(SETTLE_FRAMES);
            }
            UserEvent::Tray(event) => self.handle_tray_event(event_loop, event),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let (Some(gfx), Some(imgui)) = (self.gfx.as_ref(), self.imgui.as_mut()) else {
            return;
        };

        // Handle the event with ImGui first (window-local path)
        imgui
            .platform
            .handle_window_event(&mut imgui.context, &gfx.window, &event);

        match event {
            WindowEvent::Resized(physical_size) => {
                self.resize(physical_size);
                self.request_redraw(SETTLE_FRAMES);
            }
            WindowEvent::CloseRequested => {
                self.handle_action(event_loop, Action::HideWindow);
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.handle_action(event_loop, Action::HideWindow);
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.render() {
                    tracing::error!(error = %e, "render error");
                }
                self.sync_pipewire();
                self.sync_tray();
                if let Some(action) = self.pending_action.take() {
                    self.handle_action(event_loop, action);
                }
                if self.settle_frames > 0 {
                    self.settle_frames -= 1;
                    if let Some(gfx) = &self.gfx {
                        gfx.window.request_redraw();
                    }
                }
            }
            WindowEvent::Destroyed => {}
            // Any other input / window state change: let ImGui react, then settle.
            _ => self.request_redraw(SETTLE_FRAMES),
        }
    }
}

fn print_usage() {
    eprintln!("usage: pw-eq-imgui [--hidden]\n\n  --hidden   start minimized to the system tray");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut start_hidden = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--hidden" => start_hidden = true,
            "-h" | "--help" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    let default_audio_sink = match pw_util::get_default_audio_sink().await {
        Ok(node) => {
            tracing::info!(?node, "detected default audio sink");
            Some(node)
        }
        Err(err) => {
            tracing::error!(error = &*err, "failed to get default audio sink");
            None
        }
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    // Idle by default; input, PipeWire notifications and tray events wake us up.
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let tray = match Tray::new(proxy.clone(), !start_hidden).spawn().await {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(error = %err, "system tray unavailable; closing the window will quit");
            if start_hidden {
                tracing::warn!("--hidden ignored because there is no system tray");
                start_hidden = false;
            }
            None
        }
    };

    let mut app = App {
        proxy,
        default_audio_sink,
        start_hidden,
        tray,
        tray_state: None,
        gl_config: None,
        not_current: None,
        gfx: None,
        imgui: None,
        pipewire: None,
        settle_frames: 0,
        pending_action: None,
    };

    event_loop.run_app(&mut app).unwrap();
}
