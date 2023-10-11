#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use device_query::mouse_state::MousePosition;
use device_query::{DeviceQuery, DeviceState, Keycode, MouseState};
use egui::{self, ImageSource, Pos2, Rect, Vec2};
use egui_wgpu::renderer::ScreenDescriptor;
use egui_wgpu::{wgpu::Dx12Compiler, Renderer};
use include_dir::include_dir;
use include_dir::Dir;
use raw_window_handle::HasRawWindowHandle;
use tray_icon::{menu, menu::Menu, TrayIconBuilder};
use winit::event_loop::EventLoopBuilder;
use winit::{event::*, event_loop::ControlFlow, window::WindowLevel};

static ASSET_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/gif");
static ICON: &[u8] = include_bytes!("../assets/question.png");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pollster::block_on(run());
    Ok(())
}

enum CustomEvent {
    Animate(u8, MousePosition),
    Clear,
}

async fn run() {
    let tray_menu = Menu::new();
    tray_menu
        .append(&menu::PredefinedMenuItem::quit(Some("Quit")))
        .unwrap();
    let icon = load_icon();
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Screen pinger")
        .with_icon(icon)
        .build()
        .unwrap();

    let event_loop = EventLoopBuilder::<CustomEvent>::with_user_event().build();
    let event_loop_proxy = event_loop.create_proxy();

    let (tx, rx) = std::sync::mpsc::sync_channel(0);
    std::thread::spawn(move || {
        let device_state = DeviceState::new();

        rdev::listen(move |e: rdev::Event| match e.event_type {
            rdev::EventType::KeyPress(key) => {
                if key == rdev::Key::KeyA {
                    let mouse: MouseState = device_state.get_mouse();
                    let pos = mouse.coords;
                    // NOTE: Blocking here causes mouse to freeze
                    let _ = tx.try_send(pos);
                }
            }
            _ => {}
        })
        .unwrap();
    });

    let frame_time = 1.0 / 60.0;
    std::thread::spawn(move || loop {
        let pos = rx.recv().unwrap();
        for frame in 0..=60 {
            std::thread::sleep(std::time::Duration::from_secs_f64(frame_time));
            event_loop_proxy
                .send_event(CustomEvent::Animate(frame, pos))
                .ok();
        }
        event_loop_proxy.send_event(CustomEvent::Clear).ok();
    });

    let available_monitors = event_loop.available_monitors();
    let mut total_width = 0;
    let mut total_height = 0;

    for monitor in available_monitors {
        let monitor_size = monitor.size();
        total_width += monitor_size.width;
        total_height += monitor_size.height;
    }

    let window = winit::window::WindowBuilder::new()
        .with_inner_size(winit::dpi::PhysicalSize::new(total_width, total_height))
        .with_position(winit::dpi::PhysicalPosition::new(0.0, 0.0))
        .with_transparent(true)
        .with_decorations(false)
        .build(&event_loop)
        .unwrap();

    window.set_window_level(WindowLevel::AlwaysOnTop);
    window.set_cursor_hittest(false).unwrap();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        dx12_shader_compiler: Dx12Compiler::default(),
    });

    let surface = unsafe { instance.create_surface(&window) }.unwrap();
    // SAFETY: we windows
    unsafe {
        hide_taskbar_entry(window.raw_window_handle());
    }

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })
        .await
        .unwrap();

    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                features: wgpu::Features::empty(),
                limits: wgpu::Limits::default(),
                label: None,
            },
            None,
        )
        .await
        .unwrap();

    let size = window.inner_size();
    let mut config = surface
        .get_default_config(&adapter, size.width, size.height)
        .expect("Surface isn't supported by the adapter.");

    config.present_mode = wgpu::PresentMode::Immediate;
    surface.configure(&device, &config);

    let mut egui_state = egui_winit::State::new(&event_loop);
    let egui_context = egui::Context::default();
    egui_extras::install_image_loaders(&egui_context);
    let mut egui_renderer = Renderer::new(&device, config.format, None, 1);
    let mut my_app = MyApp::default();

    event_loop.run(move |event, _, control_flow| {
        let _ = (
            &instance,
            &adapter,
            &egui_renderer,
            &egui_context,
            &egui_state,
        );

        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(CustomEvent::Animate(frame, pos)) => {
                my_app.current_frame = Some(frame);
                my_app.mouse_position = pos;
                egui_context.request_repaint();
            }
            Event::UserEvent(CustomEvent::Clear) => {
                my_app.current_frame = None;
                egui_context.request_repaint();
            }
            Event::WindowEvent {
                event: window_event,
                ..
            } => {
                match window_event {
                    WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                    WindowEvent::Resized(new_size) => {
                        // Resize with 0 width and height is used by winit to signal a minimize event on Windows.
                        // See: https://github.com/rust-windowing/winit/issues/208
                        // This solves an issue where the app would panic when minimizing on Windows.
                        if new_size.width > 0 && new_size.height > 0 {
                            config.width = new_size.width;
                            config.height = new_size.height;
                            surface.configure(&device, &config);
                        }
                    }
                    _ => {}
                }
            }
            Event::RedrawEventsCleared => {
                window.request_redraw();
            }
            Event::RedrawRequested(_) => {
                let texture = surface.get_current_texture();
                let frame = match texture {
                    Ok(f) => f,
                    Err(e) => {
                        println!("surface lost: window is probably minimized: {e}");
                        return;
                    }
                };

                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());

                let mut encoder =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

                let input = egui_state.take_egui_input(&window);
                egui_context.begin_frame(input);
                my_app.ui(&egui_context);

                let output = egui_context.end_frame();
                let paint_jobs = egui_context.tessellate(output.shapes);
                let screen_descriptor = ScreenDescriptor {
                    size_in_pixels: [config.width, config.height],
                    pixels_per_point: 1.0,
                };

                {
                    for (id, image_delta) in &output.textures_delta.set {
                        egui_renderer.update_texture(&device, &queue, *id, image_delta);
                    }
                    for id in &output.textures_delta.free {
                        egui_renderer.free_texture(id);
                    }

                    {
                        egui_renderer.update_buffers(
                            &device,
                            &queue,
                            &mut encoder,
                            &paint_jobs,
                            &screen_descriptor,
                        );
                    }
                }

                {
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: true,
                            },
                        })],
                        depth_stencil_attachment: None,
                    });

                    egui_renderer.render(&mut render_pass, &paint_jobs, &screen_descriptor);
                }

                queue.submit(Some(encoder.finish()));
                frame.present();
            }
            _ => {}
        }
    });
}

struct MyApp {
    frames: Vec<egui::ImageSource<'static>>,
    current_frame: Option<u8>,
    mouse_position: MousePosition,
}

impl Default for MyApp {
    fn default() -> Self {
        let frames = ASSET_DIR
            .files()
            .map(|f| {
                let path = f.path().to_str().unwrap();

                ImageSource::Bytes {
                    uri: ::std::borrow::Cow::Owned(format!("bytes://{path}")),
                    bytes: egui::load::Bytes::Static(ASSET_DIR.get_file(path).unwrap().contents()),
                }
            })
            .collect::<Vec<_>>();

        Self {
            frames,
            current_frame: None,
            mouse_position: (0, 0),
        }
    }
}

impl MyApp {
    fn ui(&mut self, ctx: &egui::Context) {
        if let Some(frame) = self.current_frame {
            let current_frame = self.frames[frame as usize].clone();
            let position = Rect::from_center_size(
                Pos2::new(self.mouse_position.0 as _, self.mouse_position.1 as _),
                Vec2::new(500.0, 500.0),
            );

            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(egui::Color32::TRANSPARENT))
                .show(ctx, |ui| {
                    let img = egui::Image::new(current_frame);
                    ui.put(position, img);
                });

            ctx.request_repaint();
        }
    }
}

use raw_window_handle::RawWindowHandle;
unsafe fn hide_taskbar_entry(window_handle: RawWindowHandle) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE;

    let RawWindowHandle::Win32(raw_handle) = window_handle else {
        panic!("Unsupported platform!");
    };
    let hwnd = raw_handle.hwnd;

    let index = windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
    let style = WINDOW_EX_STYLE(0)
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_LEFT
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_LTRREADING
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_TRANSPARENT
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_WINDOWEDGE
        | windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;

    windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrA(
        HWND(hwnd as _),
        index,
        style.0 as _,
    );
}

fn load_icon() -> tray_icon::Icon {
    let (icon_rgba, icon_width, icon_height) = {
        let image = image::load_from_memory_with_format(ICON, image::ImageFormat::Png)
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };
    tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height).expect("Failed to open icon")
}
