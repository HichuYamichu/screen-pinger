#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use crossbeam::queue::ArrayQueue;
use device_query::mouse_state::MousePosition;
use device_query::{DeviceQuery, DeviceState, MouseState};
use egui::{self, ImageSource, Pos2, Rect, Vec2};
use egui_wgpu::renderer::ScreenDescriptor;
use egui_wgpu::{wgpu::Dx12Compiler, Renderer};
use include_dir::include_dir;
use include_dir::Dir;
use raw_window_handle::HasRawWindowHandle;
use rodio::{source::Source, Decoder};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tray_icon::{menu, menu::Menu, TrayIconBuilder};
use winit::event_loop::EventLoopBuilder;
use winit::{event::*, event_loop::ControlFlow, window::WindowLevel};

static ASSET_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/gif");
static ICON: &[u8] = include_bytes!("../assets/question.png");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pollster::block_on(run());
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Animation {
    id: usize,
    frame: u8,
    position: MousePosition,
    last_update: std::time::Instant,
}

enum CustomEvent {
    Animate(Animation),
    Clear(usize),
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

    let animations: Arc<ArrayQueue<Animation>> = Arc::new(ArrayQueue::new(10));
    let animations_clone = animations.clone();

    let frame_time = 1.0 / 60.0;
    let animation_driver_handle = std::thread::spawn(move || {
        let mut local_animation_queue = Vec::new();
        let animations = animations_clone;

        loop {
            // NOTE: avoid spinning with `park`
            while animations.is_empty() && local_animation_queue.is_empty() {
                std::thread::park();
            }

            while let Some(animation) = animations.pop() {
                local_animation_queue.push(animation);
            }

            for animation in local_animation_queue.iter_mut() {
                let elapsed = animation.last_update.elapsed();
                if elapsed.as_secs_f64() > frame_time {
                    animation.frame += 1;
                    animation.last_update = std::time::Instant::now();
                    if animation.frame < 60 {
                        event_loop_proxy
                            .send_event(CustomEvent::Animate(animation.clone()))
                            .ok();
                    } else {
                        event_loop_proxy
                            .send_event(CustomEvent::Clear(animation.id))
                            .ok();
                    }
                }
            }

            local_animation_queue.retain(|animation| animation.frame < 60);
        }
    });

    std::thread::spawn(move || {
        let mut primed = false;
        let device_state = DeviceState::new();
        let mut animation_id = 0;
        let (_stream, stream_handle) = rodio::OutputStream::try_default().unwrap();
        let file = BufReader::new(File::open("assets/ping_missing.ogg").unwrap());
        let source = Decoder::new(file).unwrap().buffered();

        rdev::listen(move |e: rdev::Event| match e.event_type {
            rdev::EventType::KeyPress(key) => {
                if key == rdev::Key::Alt {
                    primed = true;
                }
            }
            rdev::EventType::KeyRelease(key) => {
                if key == rdev::Key::Alt {
                    primed = false;
                }
            }
            rdev::EventType::ButtonPress(button) => {
                if primed && button == rdev::Button::Left {
                    let mouse: MouseState = device_state.get_mouse();
                    let pos = mouse.coords;
                    animation_id += 1;
                    // NOTE: Blocking here causes mouse to freeze so we do this the quick way
                    if let Ok(_) = animations.push(Animation {
                        id: animation_id,
                        frame: 0,
                        position: pos,
                        last_update: std::time::Instant::now(),
                    }) {
                        stream_handle
                            .play_raw(source.clone().convert_samples())
                            .ok();
                        animation_driver_handle.thread().unpark();
                    }
                }
            }
            _ => {}
        })
        .unwrap();
    });

    let available_monitors = event_loop.available_monitors();
    let mut offset = f32::MAX;
    let mut total_width = 0;
    let mut total_height = 0;

    for monitor in available_monitors {
        let monitor_size = monitor.size();
        total_width += monitor_size.width;
        total_height += monitor_size.height;
        let monitor_position = monitor.position();
        if (monitor_position.x as f32) < offset {
            offset = monitor_position.x as f32;
        }
    }

    let window = winit::window::WindowBuilder::new()
        .with_inner_size(winit::dpi::PhysicalSize::new(total_width, total_height))
        .with_position(winit::dpi::PhysicalPosition::new(offset, 0.0))
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
    let mut my_app = MyApp::new(offset.abs());

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
            Event::UserEvent(CustomEvent::Animate(animation)) => {
                my_app.add_animation(animation);
                egui_context.request_repaint();
            }
            Event::UserEvent(CustomEvent::Clear(animation_id)) => {
                my_app.remove_animation(animation_id);
                egui_context.request_repaint();
            }
            Event::WindowEvent {
                event: window_event,
                ..
            } => match window_event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                _ => {}
            },
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
    offset: f32,
    frames: Vec<egui::ImageSource<'static>>,
    animations: HashMap<usize, Animation>,
}

impl MyApp {
    fn new(offset: f32) -> Self {
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
            offset,
            frames,
            animations: HashMap::new(),
        }
    }
}

impl MyApp {
    fn ui(&mut self, ctx: &egui::Context) {
        for animation in self.animations.values() {
            let current_frame = self.frames[animation.frame as usize].clone();
            let position = Rect::from_center_size(
                Pos2::new(
                    animation.position.0 as f32 + self.offset,
                    animation.position.1 as _,
                ),
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

    fn add_animation(&mut self, animation: Animation) {
        self.animations.insert(animation.id, animation);
    }

    fn remove_animation(&mut self, animation_id: usize) {
        self.animations.remove(&animation_id);
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
