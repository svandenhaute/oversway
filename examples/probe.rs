use std::ptr::NonNull;
use wayland_client::{globals::registry_queue_init, Connection, Proxy, protocol::wl_surface, Dispatch, QueueHandle, globals::GlobalListContents, protocol::{wl_registry, wl_compositor}};
struct S;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S { fn event(_: &mut S, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<S>) {} }
impl Dispatch<wl_compositor::WlCompositor, ()> for S { fn event(_: &mut S, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} }
impl Dispatch<wl_surface::WlSurface, ()> for S { fn event(_: &mut S, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} }
fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, queue) = registry_queue_init::<S>(&conn).unwrap();
    for g in globals.contents().clone_list() { println!("{} v{}", g.interface, g.version); }
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
    let surface = compositor.create_surface(&qh, ());
    let t = std::time::Instant::now();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let display = raw_window_handle::RawDisplayHandle::Wayland(raw_window_handle::WaylandDisplayHandle::new(NonNull::new(conn.backend().display_ptr() as *mut _).unwrap()));
    let window = raw_window_handle::RawWindowHandle::Wayland(raw_window_handle::WaylandWindowHandle::new(NonNull::new(surface.id().as_ptr() as *mut _).unwrap()));
    let wsurface = unsafe { instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle { raw_display_handle: Some(display), raw_window_handle: window }).unwrap() };
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(&wsurface), ..Default::default() })).unwrap();
    println!("adapter {:?} in {:?}", adapter.get_info().name, t.elapsed());
    let caps = wsurface.get_capabilities(&adapter);
    println!("formats {:?}\nalpha {:?}\npresent {:?}", caps.formats, caps.alpha_modes, caps.present_modes);
}
