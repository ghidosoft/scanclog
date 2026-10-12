//! The central image view: zoom, pan and an editable crop rectangle.

use gtk::cairo;
use gtk::prelude::*;
use scanclog::imaging::{Bitmap, Crop, Raster};
use std::cell::RefCell;
use std::rc::Rc;

/// Space kept free around the image, in logical pixels.
const MARGIN: f64 = 16.0;
/// How close to an edge or corner the pointer must be to grab it.
const GRAB_DISTANCE: f64 = 8.0;
/// Drags shorter than this create no crop (they count as a click).
const MIN_DRAG: f64 = 4.0;
const HANDLE_SIZE: f64 = 7.0;
/// Zoom factor of one step (button, key or wheel notch).
const ZOOM_STEP: f64 = 1.25;
/// Zoom limits, in device pixels per image pixel.
const MIN_ZOOM: f64 = 0.02;
const MAX_ZOOM: f64 = 16.0;
/// Logical pixels panned per wheel notch.
const SCROLL_STEP: f64 = 48.0;

/// What a drag on the canvas is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragMode {
    /// Drawing a new rectangle from the press point.
    Create,
    /// Moving the whole rectangle.
    Move,
    /// Moving some edges. Each flag says whether that edge follows the pointer.
    Resize {
        left: bool,
        top: bool,
        right: bool,
        bottom: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    mode: DragMode,
    /// Crop at the start of the drag.
    original: Crop,
    /// Press point in normalized image coordinates.
    start: (f64, f64),
}

/// Image position inside the widget, in logical pixels. With zoom it can
/// extend beyond the widget.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Frame {
    fn to_image(self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.x) / self.width, (y - self.y) / self.height)
    }

    fn crop_rect(self, crop: Crop) -> (f64, f64, f64, f64) {
        let left = self.x + f64::from(crop.left) * self.width;
        let top = self.y + f64::from(crop.top) * self.height;
        let right = self.x + f64::from(crop.right) * self.width;
        let bottom = self.y + f64::from(crop.bottom) * self.height;
        (left, top, right, bottom)
    }
}

/// Zoom and pan, independent of GTK. Sizes are in logical pixels unless
/// noted; `scale_factor` converts to device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct View {
    /// Size of the full-resolution image, in its pixels.
    content: (f64, f64),
    /// Whether "fit" may enlarge the image beyond 100 %.
    fit_enlarges: bool,
    /// Device pixels per image pixel, or `None` to fit the widget.
    zoom: Option<f64>,
    /// The image point (normalized) shown at the widget center.
    center: (f64, f64),
}

impl Default for View {
    fn default() -> Self {
        View {
            content: (0.0, 0.0),
            fit_enlarges: true,
            zoom: None,
            center: (0.5, 0.5),
        }
    }
}

impl View {
    fn fit_scale(&self, widget: (f64, f64), scale_factor: f64) -> f64 {
        let available = (
            (widget.0 - 2.0 * MARGIN).max(1.0),
            (widget.1 - 2.0 * MARGIN).max(1.0),
        );
        let fit = (available.0 / self.content.0).min(available.1 / self.content.1) * scale_factor;
        if self.fit_enlarges { fit } else { fit.min(1.0) }
    }

    /// Device pixels per image pixel.
    fn scale(&self, widget: (f64, f64), scale_factor: f64) -> f64 {
        self.zoom
            .unwrap_or_else(|| self.fit_scale(widget, scale_factor))
    }

    fn frame(&self, widget: (f64, f64), scale_factor: f64) -> Option<Frame> {
        if self.content.0 < 1.0 || self.content.1 < 1.0 || widget.0 < 1.0 || widget.1 < 1.0 {
            return None;
        }
        let scale = self.scale(widget, scale_factor) / scale_factor;
        let (width, height) = (self.content.0 * scale, self.content.1 * scale);
        // Center small images; keep large ones covering the widget.
        let place = |size: f64, available: f64, center: f64| {
            if size + 2.0 * MARGIN <= available {
                ((available - size) / 2.0).round()
            } else {
                (available / 2.0 - center * size).clamp(available - MARGIN - size, MARGIN)
            }
        };
        Some(Frame {
            x: place(width, widget.0, self.center.0),
            y: place(height, widget.1, self.center.1),
            width,
            height,
        })
    }

    /// Stores the center actually shown, so panning never has to "use up"
    /// an overshoot first.
    fn settle(&mut self, widget: (f64, f64), scale_factor: f64) {
        if let Some(frame) = self.frame(widget, scale_factor) {
            self.center = (
                (widget.0 / 2.0 - frame.x) / frame.width,
                (widget.1 / 2.0 - frame.y) / frame.height,
            );
        }
    }

    /// Zooms to `zoom` keeping the image point under `anchor` in place.
    fn zoom_to(&mut self, zoom: f64, anchor: (f64, f64), widget: (f64, f64), scale_factor: f64) {
        let Some(before) = self.frame(widget, scale_factor) else {
            return;
        };
        let point = before.to_image(anchor.0, anchor.1);
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.zoom = Some(zoom);
        let size = (
            self.content.0 * zoom / scale_factor,
            self.content.1 * zoom / scale_factor,
        );
        let origin = (anchor.0 - point.0 * size.0, anchor.1 - point.1 * size.1);
        self.center = (
            (widget.0 / 2.0 - origin.0) / size.0,
            (widget.1 / 2.0 - origin.1) / size.1,
        );
        self.settle(widget, scale_factor);
    }

    fn pan(&mut self, dx: f64, dy: f64, widget: (f64, f64), scale_factor: f64) {
        let Some(frame) = self.frame(widget, scale_factor) else {
            return;
        };
        self.center.0 += dx / frame.width;
        self.center.1 += dy / frame.height;
        self.settle(widget, scale_factor);
    }

    /// The visible part of the image (normalized) and its size in device
    /// pixels at the current zoom.
    fn visible(&self, widget: (f64, f64), scale_factor: f64) -> Option<(Crop, (u32, u32))> {
        let frame = self.frame(widget, scale_factor)?;
        let left = (-frame.x / frame.width).clamp(0.0, 1.0);
        let top = (-frame.y / frame.height).clamp(0.0, 1.0);
        let right = ((widget.0 - frame.x) / frame.width).clamp(0.0, 1.0);
        let bottom = ((widget.1 - frame.y) / frame.height).clamp(0.0, 1.0);
        let region = Crop {
            left: left as f32,
            top: top as f32,
            right: right as f32,
            bottom: bottom as f32,
        };
        let scale = self.scale(widget, scale_factor);
        let pixels = |fraction: f64, size: f64| ((fraction * size * scale).round() as u32).max(1);
        let size = (
            pixels(right - left, self.content.0),
            pixels(bottom - top, self.content.1),
        );
        Some((region, size))
    }
}

/// What the canvas should render: an area of the image and the pixel size
/// it is shown at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// The visible area, normalized; [`Crop::FULL`] when all is visible.
    pub region: Crop,
    /// Size in device pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per full-resolution image pixel.
    pub scale: f64,
}

struct Image {
    surface: cairo::ImageSurface,
    /// The part of the image the surface shows.
    region: Crop,
}

#[derive(Default)]
struct State {
    image: Option<Image>,
    view: View,
    /// `None` when cropping is not available for the current image.
    crop: Option<Crop>,
    drag: Option<Drag>,
    /// View center when a middle-button pan started.
    pan_start: Option<(f64, f64)>,
    pointer: (f64, f64),
    on_crop_changed: Option<Rc<dyn Fn(Crop)>>,
    on_view_changed: Option<Rc<dyn Fn()>>,
}

#[derive(Clone)]
pub struct Canvas {
    root: gtk::Overlay,
    area: gtk::DrawingArea,
    placeholder: gtk::Label,
    zoom_controls: gtk::Box,
    zoom_label: gtk::Label,
    state: Rc<RefCell<State>>,
}

impl Canvas {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .build();
        let placeholder = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .can_target(false)
            .build();
        placeholder.add_css_class("dim-label");
        placeholder.add_css_class("title-4");

        let zoom_label = gtk::Label::builder().width_chars(5).build();
        let zoom_controls = gtk::Box::builder()
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(12)
            .margin_bottom(12)
            .visible(false)
            .build();
        zoom_controls.add_css_class("osd");
        zoom_controls.add_css_class("linked");
        let button = |icon: &str, action: &str, tooltip: &str| {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .action_name(action)
                .tooltip_text(tooltip)
                .build();
            button.add_css_class("flat");
            button
        };
        zoom_controls.append(&button(
            "zoom-out-symbolic",
            "win.zoom-out",
            "Zoom out (Ctrl+−)",
        ));
        zoom_controls.append(&zoom_label);
        zoom_controls.append(&button(
            "zoom-in-symbolic",
            "win.zoom-in",
            "Zoom in (Ctrl++)",
        ));
        zoom_controls.append(&button(
            "zoom-fit-best-symbolic",
            "win.zoom-fit",
            "Fit to window (Ctrl+0)",
        ));
        zoom_controls.append(&button(
            "zoom-original-symbolic",
            "win.zoom-actual",
            "Actual pixels (Ctrl+1)",
        ));

        let root = gtk::Overlay::builder().child(&area).build();
        root.add_overlay(&placeholder);
        root.add_overlay(&zoom_controls);
        let canvas = Canvas {
            root,
            area,
            placeholder,
            zoom_controls,
            zoom_label,
            state: Rc::new(RefCell::new(State::default())),
        };

        let this = canvas.clone();
        canvas.area.set_draw_func(move |_, cr, _, _| this.draw(cr));

        let this = canvas.clone();
        canvas
            .area
            .connect_resize(move |_, _, _| this.view_changed());

        let drag = gtk::GestureDrag::new();
        let this = canvas.clone();
        drag.connect_drag_begin(move |_, x, y| this.drag_begin(x, y));
        let this = canvas.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            if let Some((x, y)) = gesture.start_point() {
                this.drag_update(x + dx, y + dy, dx, dy);
            }
        });
        let this = canvas.clone();
        drag.connect_drag_end(move |gesture, dx, dy| {
            if let Some((x, y)) = gesture.start_point() {
                this.drag_end(x + dx, y + dy, dx, dy);
            }
        });
        canvas.area.add_controller(drag);

        // Middle button pans.
        let pan = gtk::GestureDrag::builder().button(2).build();
        let this = canvas.clone();
        pan.connect_drag_begin(move |_, _, _| {
            let mut state = this.state.borrow_mut();
            state.pan_start = Some(state.view.center);
        });
        let this = canvas.clone();
        pan.connect_drag_update(move |_, dx, dy| this.pan_from_start(dx, dy));
        canvas.area.add_controller(pan);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let this = canvas.clone();
        scroll.connect_scroll(move |controller, dx, dy| {
            let modifiers = controller.current_event_state();
            if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                this.zoom_by(ZOOM_STEP.powf(-dy), None);
            } else if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                this.pan_by((dx + dy) * SCROLL_STEP, 0.0);
            } else {
                this.pan_by(dx * SCROLL_STEP, dy * SCROLL_STEP);
            }
            gtk::glib::Propagation::Stop
        });
        canvas.area.add_controller(scroll);

        let motion = gtk::EventControllerMotion::new();
        let this = canvas.clone();
        motion.connect_motion(move |_, x, y| {
            this.state.borrow_mut().pointer = (x, y);
            this.update_cursor(x, y);
        });
        canvas.area.add_controller(motion);

        canvas
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.root
    }

    fn widget_size(&self) -> (f64, f64) {
        (f64::from(self.area.width()), f64::from(self.area.height()))
    }

    fn scale_factor(&self) -> f64 {
        f64::from(self.area.scale_factor().max(1))
    }

    /// Starts showing an image of `width × height` full-resolution pixels,
    /// fitted to the widget. `fit_enlarges` allows fitting above 100 %.
    pub fn reset_view(&self, width: u32, height: u32, fit_enlarges: bool) {
        self.state.borrow_mut().view = View {
            content: (f64::from(width), f64::from(height)),
            fit_enlarges,
            ..View::default()
        };
        self.view_changed();
    }

    /// Changes the image size (e.g. after a rotation), keeping the zoom.
    pub fn set_content_size(&self, width: u32, height: u32) {
        let content = (f64::from(width), f64::from(height));
        {
            let mut state = self.state.borrow_mut();
            if state.view.content == content {
                return;
            }
            state.view.content = content;
        }
        self.view_changed();
    }

    /// What should be rendered for the current zoom and widget size.
    pub fn viewport(&self) -> Option<Viewport> {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        let view = self.state.borrow().view;
        let (region, (width, height)) = view.visible(widget, scale_factor)?;
        Some(Viewport {
            region,
            width,
            height,
            scale: view.scale(widget, scale_factor),
        })
    }

    pub fn zoom_in(&self) {
        self.zoom_by(ZOOM_STEP, Some(self.center_point()));
    }

    pub fn zoom_out(&self) {
        self.zoom_by(1.0 / ZOOM_STEP, Some(self.center_point()));
    }

    pub fn zoom_fit(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.view.zoom = None;
            state.view.center = (0.5, 0.5);
        }
        self.view_changed();
    }

    pub fn zoom_actual(&self) {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        let anchor = self.center_point();
        self.state
            .borrow_mut()
            .view
            .zoom_to(1.0, anchor, widget, scale_factor);
        self.view_changed();
    }

    fn center_point(&self) -> (f64, f64) {
        let (width, height) = self.widget_size();
        (width / 2.0, height / 2.0)
    }

    /// Multiplies the zoom, keeping the point under `anchor` (or the pointer) still.
    fn zoom_by(&self, factor: f64, anchor: Option<(f64, f64)>) {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        {
            let mut state = self.state.borrow_mut();
            if state.image.is_none() {
                return;
            }
            let anchor = anchor.unwrap_or(state.pointer);
            let zoom = state.view.scale(widget, scale_factor) * factor;
            state.view.zoom_to(zoom, anchor, widget, scale_factor);
        }
        self.view_changed();
    }

    fn pan_by(&self, dx: f64, dy: f64) {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        let changed = {
            let mut state = self.state.borrow_mut();
            let before = state.view.center;
            state.view.pan(dx, dy, widget, scale_factor);
            state.view.center != before
        };
        if changed {
            self.view_changed();
        }
    }

    fn pan_from_start(&self, dx: f64, dy: f64) {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        {
            let mut state = self.state.borrow_mut();
            let Some(start) = state.pan_start else { return };
            state.view.center = start;
            state.view.pan(-dx, -dy, widget, scale_factor);
        }
        self.view_changed();
    }

    fn view_changed(&self) {
        let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
        let (scale, has_image) = {
            let state = self.state.borrow();
            (
                state.view.scale(widget, scale_factor),
                state.image.is_some(),
            )
        };
        self.zoom_label.set_text(&format!("{:.0} %", scale * 100.0));
        self.zoom_controls.set_visible(has_image);
        self.area.queue_draw();
        let callback = self.state.borrow().on_view_changed.clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Shows `bitmap`, which renders `region` of the image.
    pub fn set_image(&self, bitmap: Option<(&Bitmap, Crop)>) {
        let had_image = self.state.borrow().image.is_some();
        self.state.borrow_mut().image = bitmap.map(|(bitmap, region)| Image {
            surface: surface_from_bitmap(bitmap),
            region,
        });
        let has_image = self.state.borrow().image.is_some();
        self.update_placeholder();
        self.area.queue_draw();
        if has_image != had_image {
            let (widget, scale_factor) = (self.widget_size(), self.scale_factor());
            let scale = self.state.borrow().view.scale(widget, scale_factor);
            self.zoom_label.set_text(&format!("{:.0} %", scale * 100.0));
            self.zoom_controls.set_visible(has_image);
        }
    }

    /// Text shown when there is no image.
    pub fn set_placeholder(&self, text: &str) {
        self.placeholder.set_text(text);
        self.update_placeholder();
    }

    fn update_placeholder(&self) {
        let has_image = self.state.borrow().image.is_some();
        self.placeholder
            .set_visible(!has_image && !self.placeholder.text().is_empty());
    }

    /// Shows `crop` as editable, or disables cropping with `None`.
    pub fn set_crop(&self, crop: Option<Crop>) {
        let mut state = self.state.borrow_mut();
        state.crop = crop;
        state.drag = None;
        drop(state);
        self.area.queue_draw();
    }

    /// Shows `crop` unless the user is dragging, so a background update
    /// never interrupts an edit in progress.
    pub fn sync_crop(&self, crop: Crop) {
        let mut state = self.state.borrow_mut();
        if state.drag.is_some() || state.crop == Some(crop) {
            return;
        }
        state.crop = Some(crop);
        drop(state);
        self.area.queue_draw();
    }

    /// Whether cropping is currently enabled.
    pub fn has_crop(&self) -> bool {
        self.state.borrow().crop.is_some()
    }

    /// Called when the user finishes changing the crop.
    pub fn connect_crop_changed(&self, callback: impl Fn(Crop) + 'static) {
        self.state.borrow_mut().on_crop_changed = Some(Rc::new(callback));
    }

    /// Called when the visible area changes (resize, zoom or pan), so the
    /// image can be re-rendered.
    pub fn connect_view_changed(&self, callback: impl Fn() + 'static) {
        self.state.borrow_mut().on_view_changed = Some(Rc::new(callback));
    }

    fn frame(&self) -> Option<Frame> {
        let state = self.state.borrow();
        state.image.as_ref()?;
        state.view.frame(self.widget_size(), self.scale_factor())
    }

    fn draw(&self, cr: &cairo::Context) {
        let Some(frame) = self.frame() else { return };
        let state = self.state.borrow();
        let Some(image) = &state.image else { return };

        // Soft shadow so white pages stand out from a light background.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
        cr.rectangle(frame.x + 1.0, frame.y + 2.0, frame.width, frame.height);
        let _ = cr.fill();

        // The surface may show only part of the image (when zoomed), and may
        // be from a previous zoom level until the new render arrives.
        let (left, top, right, bottom) = frame.crop_rect(image.region);
        let _ = cr.save();
        cr.rectangle(frame.x, frame.y, frame.width, frame.height);
        cr.clip();
        cr.translate(left, top);
        let sx = (right - left) / f64::from(image.surface.width());
        let sy = (bottom - top) / f64::from(image.surface.height());
        cr.scale(sx, sy);
        let _ = cr.set_source_surface(&image.surface, 0.0, 0.0);
        let device_scale = sx * self.scale_factor();
        let filter = if device_scale > 1.5 {
            // Enlarged pixels stay crisp when zoomed in.
            cairo::Filter::Nearest
        } else if device_scale > 1.0 {
            cairo::Filter::Good
        } else {
            cairo::Filter::Best
        };
        cr.source().set_filter(filter);
        let _ = cr.paint();
        let _ = cr.restore();

        if let Some(crop) = state.crop.filter(|c| !c.is_full()) {
            draw_crop(cr, frame, crop);
        }
    }

    /// What a press at `(x, y)` would grab.
    fn hit_test(&self, x: f64, y: f64) -> Option<DragMode> {
        let frame = self.frame()?;
        let crop = self.state.borrow().crop?;
        if crop.is_full() {
            return Some(DragMode::Create);
        }
        let (left, top, right, bottom) = frame.crop_rect(crop);
        let within_y = y > top - GRAB_DISTANCE && y < bottom + GRAB_DISTANCE;
        let within_x = x > left - GRAB_DISTANCE && x < right + GRAB_DISTANCE;
        let near = |a: f64, b: f64| (a - b).abs() <= GRAB_DISTANCE;
        let grab_left = near(x, left) && within_y;
        let grab_top = near(y, top) && within_x;
        // On tiny rectangles prefer the left/top edge over the opposite one.
        let grab_right = near(x, right) && within_y && !grab_left;
        let grab_bottom = near(y, bottom) && within_x && !grab_top;
        if grab_left || grab_top || grab_right || grab_bottom {
            return Some(DragMode::Resize {
                left: grab_left,
                top: grab_top,
                right: grab_right,
                bottom: grab_bottom,
            });
        }
        if x > left && x < right && y > top && y < bottom {
            Some(DragMode::Move)
        } else {
            Some(DragMode::Create)
        }
    }

    fn drag_begin(&self, x: f64, y: f64) {
        self.area.grab_focus();
        let (Some(mode), Some(frame)) = (self.hit_test(x, y), self.frame()) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        let Some(original) = state.crop else { return };
        state.drag = Some(Drag {
            mode,
            original,
            start: frame.to_image(x, y),
        });
    }

    fn drag_update(&self, x: f64, y: f64, dx: f64, dy: f64) {
        let Some(frame) = self.frame() else { return };
        let mut state = self.state.borrow_mut();
        let Some(drag) = state.drag else { return };
        if drag.mode == DragMode::Create && dx.hypot(dy) < MIN_DRAG {
            return;
        }
        state.crop = Some(apply_drag(drag, frame.to_image(x, y)));
        drop(state);
        self.area.queue_draw();
    }

    fn drag_end(&self, x: f64, y: f64, dx: f64, dy: f64) {
        let Some(frame) = self.frame() else { return };
        let mut state = self.state.borrow_mut();
        let Some(drag) = state.drag.take() else {
            return;
        };
        let crop = if dx.hypot(dy) < MIN_DRAG {
            // A click outside the rectangle clears it; other clicks change nothing.
            if drag.mode == DragMode::Create {
                Crop::FULL
            } else {
                drag.original
            }
        } else {
            apply_drag(drag, frame.to_image(x, y))
        };
        state.crop = Some(crop);
        let callback = state.on_crop_changed.clone();
        drop(state);
        self.area.queue_draw();
        if crop != drag.original
            && let Some(callback) = callback
        {
            callback(crop);
        }
    }

    fn update_cursor(&self, x: f64, y: f64) {
        if self.state.borrow().drag.is_some() {
            return;
        }
        let name = match self.hit_test(x, y) {
            None => "default",
            Some(DragMode::Create) => "crosshair",
            Some(DragMode::Move) => "move",
            Some(DragMode::Resize {
                left,
                top,
                right,
                bottom,
            }) => match (left || right, top || bottom) {
                (true, true) if (left && top) || (right && bottom) => "nwse-resize",
                (true, true) => "nesw-resize",
                (true, false) => "ew-resize",
                _ => "ns-resize",
            },
        };
        self.area.set_cursor_from_name(Some(name));
    }
}

/// The crop resulting from a drag that is now at `point` (normalized).
fn apply_drag(drag: Drag, point: (f64, f64)) -> Crop {
    let (px, py) = (point.0 as f32, point.1 as f32);
    let (sx, sy) = (drag.start.0 as f32, drag.start.1 as f32);
    let original = drag.original;
    match drag.mode {
        DragMode::Create => Crop::from_corners((sx, sy), (px, py)),
        DragMode::Move => {
            // Keep the size and stay inside the image.
            let dx = (px - sx).clamp(-original.left, 1.0 - original.right);
            let dy = (py - sy).clamp(-original.top, 1.0 - original.bottom);
            Crop {
                left: original.left + dx,
                top: original.top + dy,
                right: original.right + dx,
                bottom: original.bottom + dy,
            }
        }
        DragMode::Resize {
            left,
            top,
            right,
            bottom,
        } => {
            let pick = |moves: bool, edge: f32, value: f32| if moves { value } else { edge };
            Crop::from_corners(
                (pick(left, original.left, px), pick(top, original.top, py)),
                (
                    pick(right, original.right, px),
                    pick(bottom, original.bottom, py),
                ),
            )
        }
    }
}

fn draw_crop(cr: &cairo::Context, frame: Frame, crop: Crop) {
    let (left, top, right, bottom) = frame.crop_rect(crop);

    // Dim everything outside the crop.
    cr.set_fill_rule(cairo::FillRule::EvenOdd);
    cr.rectangle(frame.x, frame.y, frame.width, frame.height);
    cr.rectangle(left, top, right - left, bottom - top);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
    let _ = cr.fill();

    // A dark and a light line keep the border visible on any content.
    cr.set_line_width(1.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
    cr.rectangle(
        left - 0.5,
        top - 0.5,
        right - left + 1.0,
        bottom - top + 1.0,
    );
    let _ = cr.stroke();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.rectangle(
        left + 0.5,
        top + 0.5,
        right - left - 1.0,
        bottom - top - 1.0,
    );
    let _ = cr.stroke();

    let mid_x = (left + right) / 2.0;
    let mid_y = (top + bottom) / 2.0;
    for (x, y) in [
        (left, top),
        (right, top),
        (left, bottom),
        (right, bottom),
        (mid_x, top),
        (mid_x, bottom),
        (left, mid_y),
        (right, mid_y),
    ] {
        cr.rectangle(
            x - HANDLE_SIZE / 2.0,
            y - HANDLE_SIZE / 2.0,
            HANDLE_SIZE,
            HANDLE_SIZE,
        );
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
        let _ = cr.stroke();
    }
}

/// Converts a bitmap to an opaque cairo surface (native-endian xRGB).
fn surface_from_bitmap(bitmap: &Bitmap) -> cairo::ImageSurface {
    let (width, height) = bitmap.raster.dimensions();
    let format = cairo::Format::Rgb24;
    let stride = format
        .stride_for_width(width)
        .expect("image width fits cairo limits") as usize;
    let mut data = vec![0u8; stride * height as usize];
    let pixel =
        |r: u8, g: u8, b: u8| (u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)).to_ne_bytes();
    let (raw, channels) = match &bitmap.raster {
        Raster::Rgb(image) => (image.as_raw(), 3),
        Raster::Gray(image) | Raster::Bilevel(image) => (image.as_raw(), 1),
    };
    let row_len = width as usize * channels;
    for (out_row, in_row) in data.chunks_exact_mut(stride).zip(raw.chunks_exact(row_len)) {
        for (out, p) in out_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(in_row.chunks_exact(channels))
        {
            let (r, g, b) = if channels == 3 {
                (p[0], p[1], p[2])
            } else {
                (p[0], p[0], p[0])
            };
            out.copy_from_slice(&pixel(r, g, b));
        }
    }
    cairo::ImageSurface::create_for_data(data, format, width as i32, height as i32, stride as i32)
        .expect("valid surface parameters")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(mode: DragMode) -> Drag {
        Drag {
            mode,
            original: Crop {
                left: 0.2,
                top: 0.2,
                right: 0.6,
                bottom: 0.6,
            },
            start: (0.4, 0.4),
        }
    }

    #[test]
    fn moving_keeps_size_and_stays_inside() {
        let crop = apply_drag(drag(DragMode::Move), (0.9, 0.0));
        assert!((crop.right - 1.0).abs() < 1e-6 && crop.top.abs() < 1e-6);
        assert!((crop.width() - 0.4).abs() < 1e-6 && (crop.height() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn resizing_moves_only_grabbed_edges() {
        let mode = DragMode::Resize {
            left: false,
            top: false,
            right: true,
            bottom: true,
        };
        let crop = apply_drag(drag(mode), (0.8, 0.9));
        assert_eq!(
            crop,
            Crop {
                left: 0.2,
                top: 0.2,
                right: 0.8,
                bottom: 0.9
            }
        );
        // Dragging an edge past its opposite flips the rectangle.
        let mode = DragMode::Resize {
            left: true,
            top: false,
            right: false,
            bottom: false,
        };
        let crop = apply_drag(drag(mode), (0.7, 0.5));
        assert!((crop.left - 0.6).abs() < 1e-6 && (crop.right - 0.7).abs() < 1e-6);
    }

    #[test]
    fn creating_spans_start_and_pointer() {
        let crop = apply_drag(drag(DragMode::Create), (0.1, 1.5));
        assert_eq!(
            crop,
            Crop {
                left: 0.1,
                top: 0.4,
                right: 0.4,
                bottom: 1.0
            }
        );
    }

    const WIDGET: (f64, f64) = (800.0, 600.0);

    fn view(content: (f64, f64), fit_enlarges: bool) -> View {
        View {
            content,
            fit_enlarges,
            ..View::default()
        }
    }

    #[test]
    fn fit_does_not_enlarge_pages() {
        let small = view((300.0, 200.0), false);
        let frame = small.frame(WIDGET, 1.0).unwrap();
        assert_eq!(
            (frame.width, frame.height),
            (300.0, 200.0),
            "shown at 100 %"
        );
        assert_eq!((frame.x, frame.y), (250.0, 200.0), "centered");
        let preview = view((300.0, 200.0), true);
        assert!(
            preview.frame(WIDGET, 1.0).unwrap().width > 700.0,
            "previews fill the space"
        );
        // On a HiDPI screen, 100 % means one device pixel per image pixel.
        assert_eq!(small.frame(WIDGET, 2.0).unwrap().width, 150.0);
    }

    #[test]
    fn zooming_keeps_the_anchor_still_and_reports_the_visible_region() {
        let mut v = view((2000.0, 3000.0), false);
        let anchor = (300.0, 200.0);
        let before = v.frame(WIDGET, 1.0).unwrap().to_image(anchor.0, anchor.1);
        v.zoom_to(1.0, anchor, WIDGET, 1.0);
        let after = v.frame(WIDGET, 1.0).unwrap().to_image(anchor.0, anchor.1);
        assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9);

        let (region, size) = v.visible(WIDGET, 1.0).unwrap();
        assert_eq!(
            size,
            (800, 600),
            "the visible area fills the widget at 100 %"
        );
        assert!((f64::from(region.width()) * 2000.0 - 800.0).abs() < 1.0);
    }

    #[test]
    fn panning_stops_at_the_edges() {
        let mut v = view((2000.0, 3000.0), false);
        v.zoom_to(1.0, (400.0, 300.0), WIDGET, 1.0);
        v.pan(-100_000.0, -100_000.0, WIDGET, 1.0);
        let frame = v.frame(WIDGET, 1.0).unwrap();
        assert_eq!(
            (frame.x, frame.y),
            (MARGIN, MARGIN),
            "top-left corner reached"
        );
        v.pan(100_000.0, 0.0, WIDGET, 1.0);
        let frame = v.frame(WIDGET, 1.0).unwrap();
        assert_eq!(
            frame.x + frame.width,
            WIDGET.0 - MARGIN,
            "right edge reached"
        );
    }
}
