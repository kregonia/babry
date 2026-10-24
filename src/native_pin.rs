//! Native wl_shm Pin surface. It shares the editor's pixel type and does not
//! create a second GUI toolkit instance.
use crate::{
    compositor::{self, Backend},
    pixels::{self, RgbaPixels},
};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport, wp_viewporter,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use std::path::Path;

const LEFT: u32 = 0x110;
const RIGHT: u32 = 0x111;
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Drag {
    grab: (f64, f64),
    origin: (i32, i32),
    backend: Backend,
}

pub fn run_surface(path: &Path, output: Option<String>) -> Result<(), String> {
    let source = pixels::read_png(path)?;
    let outputs = compositor::outputs()?;
    let selected = compositor::select_output(&outputs, output.as_deref())?;
    let (mut pin, mut queue) = Pin::new(source, selected)?;
    while !pin.exit {
        queue
            .blocking_dispatch(&mut pin)
            .map_err(|e| format!("Pin Wayland 事件失败: {e}"))?;
    }
    Ok(())
}

#[allow(dead_code)]
struct Pin { registry_state:RegistryState, seat_state:SeatState, output_state:OutputState, shm:Shm, layer:LayerSurface, pool:SlotPool, source:RgbaPixels, display:RgbaPixels, buffer:Option<Buffer>, viewport:Option<smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport>, _viewporter:Option<smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter>, x:i32,y:i32,width:u32,height:u32,screen_width:u32,screen_height:u32,drag:Option<Drag>,pointer:Option<wl_pointer::WlPointer>,keyboard:Option<wl_keyboard::WlKeyboard>,pending:bool,first:bool,exit:bool,modifiers:Modifiers }
impl Pin {
    fn new(
        source: RgbaPixels,
        output: compositor::OutputInfo,
    ) -> Result<
        (
            Self,
            smithay_client_toolkit::reexports::client::EventQueue<Self>,
        ),
        String,
    > {
        let logical_w = (source.width() as f32 / output.scale).round().max(1.) as u32;
        let logical_h = (source.height() as f32 / output.scale).round().max(1.) as u32;
        let fit = (output.width as f32 / logical_w as f32)
            .min(output.height as f32 / logical_h as f32)
            .min(1.);
        let width = (logical_w as f32 * fit).round().max(1.) as u32;
        let height = (logical_h as f32 * fit).round().max(1.) as u32;
        let display = source.resize_nearest(width, height)?;
        let x = ((output.width - width) / 2) as i32;
        let y = ((output.height - height) / 2) as i32;
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, queue) = registry_queue_init::<Self>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let comp = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
        let shell = LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
        let surface = comp.create_surface(&qh);
        let manager:Option<smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter>=globals.bind(&qh,1..=1,()).ok();
        let viewport = manager.as_ref().map(|m| m.get_viewport(&surface, &qh, ()));
        if let Some(v) = &viewport {
            v.set_destination(width as i32, height as i32);
        }
        let layer = shell.create_layer_surface(&qh, surface, Layer::Top, Some("babry-pin"), None);
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(width, height);
        layer.set_margin(y, 0, 0, x);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.commit();
        let pool = SlotPool::new(source.size.bytes().max(4), &shm).map_err(|e| e.to_string())?;
        Ok((
            Self {
                registry_state: RegistryState::new(&globals),
                seat_state: SeatState::new(&globals, &qh),
                output_state: OutputState::new(&globals, &qh),
                shm,
                layer,
                pool,
                source,
                display,
                buffer: None,
                viewport,
                _viewporter: manager,
                x,
                y,
                width,
                height,
                screen_width: output.width,
                screen_height: output.height,
                drag: None,
                pointer: None,
                keyboard: None,
                pending: true,
                first: true,
                exit: false,
                modifiers: Modifiers::default(),
            },
            queue,
        ))
    }
    fn draw(&mut self) {
        if self.viewport.is_some() && self.buffer.is_some() {
            self.pending = false;
            return;
        }
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            self.display.width() as i32,
            self.display.height() as i32,
            self.display.size.stride as i32,
            smithay_client_toolkit::reexports::client::protocol::wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        let _ = crate::native::copy_rgba_to_argb8888(&self.display.bytes, canvas);
        let _ = buffer.attach_to(self.layer.wl_surface());
        self.layer.wl_surface().damage_buffer(
            0,
            0,
            self.display.width() as i32,
            self.display.height() as i32,
        );
        self.layer.commit();
        self.buffer = Some(buffer);
        self.pending = false;
    }
    fn move_to(&mut self, x: f64, y: f64) {
        let mx = self.screen_width.saturating_sub(self.width) as i32;
        let my = self.screen_height.saturating_sub(self.height) as i32;
        let nx = (x.round() as i32).clamp(0, mx);
        let ny = (y.round() as i32).clamp(0, my);
        if (nx, ny) == (self.x, self.y) {
            return;
        }
        self.x = nx;
        self.y = ny;
        self.layer.set_margin(ny, 0, 0, nx);
        self.layer.commit();
    }
}
impl CompositorHandler for Pin {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Pin {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl SeatHandler for Pin {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok()
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok()
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer {
            self.pointer = None
        }
        if cap == Capability::Keyboard {
            self.keyboard = None
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
impl KeyboardHandler for Pin {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.exit = true
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        m: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = m
    }
}
impl PointerHandler for Pin {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            if e.surface != *self.layer.wl_surface() {
                continue;
            }
            match e.kind {
                PointerEventKind::Press { button: LEFT, .. } => {
                    self.drag = Some(Drag {
                        grab: e.position,
                        origin: (self.x, self.y),
                        backend: Backend::detect().unwrap_or(Backend::Niri),
                    });
                }
                PointerEventKind::Release { button: LEFT, .. } => self.drag = None,
                PointerEventKind::Press { button: RIGHT, .. } => self.exit = true,
                PointerEventKind::Motion { .. } => {
                    if let Some(d) = self.drag {
                        let (dx, dy) = if d.backend == Backend::Niri {
                            (e.position.0 - d.grab.0, e.position.1 - d.grab.1)
                        } else {
                            (
                                self.x as f64 + e.position.0 - d.origin.0 as f64 - d.grab.0,
                                self.y as f64 + e.position.1 - d.origin.1 as f64 - d.grab.1,
                            )
                        };
                        self.move_to(d.origin.0 as f64 + dx, d.origin.1 as f64 + dy)
                    }
                }
                _ => {}
            }
        }
        if self.pending || self.first {
            self.draw();
            self.first = false;
        }
    }
}
impl LayerShellHandler for Pin {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.pending = true
    }
}
impl ShmHandler for Pin {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_compositor!(Pin);
delegate_output!(Pin);
delegate_shm!(Pin);
delegate_noop!(Pin: ignore wp_viewporter::WpViewporter);
delegate_noop!(Pin: ignore wp_viewport::WpViewport);
delegate_seat!(Pin);
delegate_keyboard!(Pin);
delegate_pointer!(Pin);
delegate_layer!(Pin);
delegate_registry!(Pin);
impl ProvidesRegistryState for Pin {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
