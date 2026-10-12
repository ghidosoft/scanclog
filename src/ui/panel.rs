//! The settings panel: scan parameters and per-page adjustments.

use gtk::prelude::*;
use scanclog::imaging::{Adjustments, MAX_SKEW, OutputMode, Threshold};
use scanclog::scanning::PaperSize;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Scan parameters, shown only while the preview entry is selected.
#[derive(Debug, Clone, PartialEq)]
pub struct Acquisition {
    pub resolutions: Vec<u32>,
    pub resolution: u32,
    /// Shown for the document feeder, which has no preview to crop on.
    pub paper: Option<PaperSize>,
}

/// Everything the panel displays.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelState {
    pub adjustments: Adjustments,
    pub allowed_modes: Vec<OutputMode>,
    /// Whether the straightening control applies (pages only).
    pub can_straighten: bool,
    /// Physical size of the crop, or `None` when the whole area is used.
    pub crop_size_mm: Option<(f32, f32)>,
    pub acquisition: Option<Acquisition>,
    /// The threshold computed for automatic mode, once known.
    pub auto_threshold: Option<u8>,
}

/// A change made by the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelEvent {
    Adjustments(Adjustments),
    Resolution(u32),
    Paper(PaperSize),
}

type EventHandler = Box<dyn Fn(PanelEvent)>;

/// Writes a slider value (in percent) into the adjustments.
type ScaleSetter = fn(&mut Adjustments, f32);

struct Widgets {
    progress_box: gtk::Box,
    progress: gtk::ProgressBar,
    scan_section: gtk::Box,
    resolution: gtk::DropDown,
    paper_label: gtk::Label,
    paper: gtk::DropDown,
    mode: gtk::DropDown,
    straighten_label: gtk::Label,
    straighten_box: gtk::Box,
    straighten: gtk::Scale,
    crop_label: gtk::Label,
    brightness: gtk::Scale,
    contrast: gtk::Scale,
    sharpen: gtk::Scale,
    threshold_label: gtk::Label,
    threshold: gtk::Scale,
    auto_threshold: gtk::CheckButton,
}

#[derive(Clone)]
pub struct Panel {
    root: gtk::Box,
    widgets: Rc<Widgets>,
    /// The state being edited; user changes are applied here first.
    state: Rc<RefCell<Option<PanelState>>>,
    /// The state the widgets were last updated to.
    displayed: Rc<RefCell<Option<PanelState>>>,
    /// Set while widgets are updated from code, to suppress events.
    updating: Rc<Cell<bool>>,
    on_event: Rc<RefCell<Option<EventHandler>>>,
}

impl Panel {
    pub fn new() -> Self {
        let resolution = gtk::DropDown::from_strings(&[]);
        let paper_labels: Vec<&str> = PaperSize::ALL.iter().map(|p| p.label()).collect();
        let paper = gtk::DropDown::from_strings(&paper_labels);
        let paper_label = field_label("Paper size");
        let scan_grid = grid();
        attach_row(&scan_grid, 0, &field_label("Resolution"), &resolution);
        attach_row(&scan_grid, 1, &paper_label, &paper);
        let scan_section = section("Scan", &scan_grid);

        let mode = gtk::DropDown::from_strings(&[]);
        let crop_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        let crop_reset = gtk::Button::builder()
            .label("Reset")
            .action_name("win.reset-crop")
            .tooltip_text("Use the whole area")
            .build();
        let crop_detect = gtk::Button::builder()
            .label("Auto")
            .action_name("win.detect-crop")
            .tooltip_text("Find the document on the glass")
            .build();
        let crop_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        crop_box.append(&crop_label);
        crop_box.append(&crop_detect);
        crop_box.append(&crop_reset);
        let straighten = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            -f64::from(MAX_SKEW),
            f64::from(MAX_SKEW),
            0.1,
        );
        straighten.set_digits(1);
        straighten.set_draw_value(true);
        straighten.set_value_pos(gtk::PositionType::Right);
        straighten.add_mark(0.0, gtk::PositionType::Bottom, None);
        straighten.set_hexpand(true);
        straighten.set_tooltip_text(Some("Fine rotation in degrees, clockwise"));
        let straighten_auto = gtk::Button::builder()
            .label("Auto")
            .action_name("win.auto-straighten")
            .tooltip_text("Detect the tilt of the page")
            .valign(gtk::Align::Center)
            .build();
        let straighten_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        straighten_box.append(&straighten);
        straighten_box.append(&straighten_auto);
        let straighten_label = field_label("Straighten");
        let rotate_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for (icon, action, tooltip) in [
            (
                "object-rotate-left-symbolic",
                "win.rotate-left",
                "Rotate left (Ctrl+L)",
            ),
            (
                "object-rotate-right-symbolic",
                "win.rotate-right",
                "Rotate right (Ctrl+R)",
            ),
        ] {
            rotate_box.append(
                &gtk::Button::builder()
                    .icon_name(icon)
                    .action_name(action)
                    .tooltip_text(tooltip)
                    .build(),
            );
        }
        let image_grid = grid();
        attach_row(&image_grid, 0, &field_label("Mode"), &mode);
        attach_row(&image_grid, 1, &field_label("Rotate"), &rotate_box);
        attach_row(&image_grid, 2, &straighten_label, &straighten_box);
        attach_row(&image_grid, 3, &field_label("Crop"), &crop_box);
        let image_section = section("Image", &image_grid);

        let brightness = slider(-100.0, 100.0, 0.0);
        let contrast = slider(-100.0, 100.0, 0.0);
        let sharpen = slider(0.0, 200.0, 0.0);
        let threshold = slider(0.0, 255.0, 128.0);
        let threshold_label = field_label("Threshold");
        let auto_threshold = gtk::CheckButton::with_label("Automatic");
        let adjust_grid = grid();
        attach_row(&adjust_grid, 0, &field_label("Brightness"), &brightness);
        attach_row(&adjust_grid, 1, &field_label("Contrast"), &contrast);
        attach_row(&adjust_grid, 2, &field_label("Sharpen"), &sharpen);
        attach_row(&adjust_grid, 3, &threshold_label, &threshold);
        adjust_grid.attach(&auto_threshold, 1, 4, 1, 1);
        let reset = gtk::Button::builder()
            .label("Reset Adjustments")
            .action_name("win.reset-adjustments")
            .halign(gtk::Align::End)
            .margin_top(6)
            .build();
        adjust_grid.attach(&reset, 0, 5, 2, 1);
        let adjust_section = section("Adjustments", &adjust_grid);

        let sections = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        sections.append(&scan_section);
        sections.append(&image_section);
        sections.append(&adjust_section);
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&sections)
            .build();

        let preview = gtk::Button::builder()
            .label("_Preview")
            .use_underline(true)
            .action_name("win.preview")
            .hexpand(true)
            .tooltip_text("Quick low-resolution scan of the flatbed (Ctrl+P)")
            .build();
        let scan = gtk::Button::builder()
            .label("_Scan")
            .use_underline(true)
            .action_name("win.scan")
            .hexpand(true)
            .tooltip_text("Scan with these settings and add a page (Ctrl+Return)")
            .build();
        scan.add_css_class("suggested-action");
        let actions = gtk::Box::builder()
            .spacing(8)
            .homogeneous(true)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        actions.append(&preview);
        actions.append(&scan);

        let progress = gtk::ProgressBar::builder()
            .hexpand(true)
            .valign(gtk::Align::Center)
            .show_text(true)
            .tooltip_text("Estimated from previous scans")
            .build();
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .action_name("win.cancel")
            .tooltip_text("Stop the scan (Esc)")
            .build();
        let progress_box = gtk::Box::builder()
            .spacing(8)
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .visible(false)
            .build();
        progress_box.append(&progress);
        progress_box.append(&cancel);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(320);
        root.append(&scrolled);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&progress_box);
        root.append(&actions);

        let panel = Panel {
            root,
            widgets: Rc::new(Widgets {
                progress_box,
                progress,
                scan_section,
                resolution,
                paper_label,
                paper,
                mode,
                straighten_label,
                straighten_box,
                straighten,
                crop_label,
                brightness,
                contrast,
                sharpen,
                threshold_label,
                threshold,
                auto_threshold,
            }),
            state: Rc::default(),
            displayed: Rc::default(),
            updating: Rc::default(),
            on_event: Rc::default(),
        };
        panel.connect_signals();
        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Shows scan progress (0..1) with a label, or hides it with `None`.
    pub fn set_progress(&self, progress: Option<(f64, &str)>) {
        let w = &self.widgets;
        w.progress_box.set_visible(progress.is_some());
        if let Some((fraction, text)) = progress {
            w.progress.set_fraction(fraction);
            w.progress.set_text(Some(text));
        }
    }

    pub fn connect_event(&self, callback: impl Fn(PanelEvent) + 'static) {
        *self.on_event.borrow_mut() = Some(Box::new(callback));
    }

    /// Updates every control to `state` without emitting events.
    pub fn show(&self, state: PanelState) {
        let previous = self.displayed.borrow().clone();
        if previous.as_ref() == Some(&state) {
            return;
        }
        self.updating.set(true);
        let w = &self.widgets;
        let a = &state.adjustments;
        let previous_resolutions = previous
            .as_ref()
            .and_then(|p| p.acquisition.as_ref())
            .map(|a| &a.resolutions);
        let previous_modes = previous.as_ref().map(|p| &p.allowed_modes);

        match &state.acquisition {
            Some(acquisition) => {
                w.scan_section.set_visible(true);
                let labels: Vec<String> = acquisition
                    .resolutions
                    .iter()
                    .map(|r| format!("{r} dpi"))
                    .collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                // Replacing the model closes an open popup, so only do it
                // when the choices really change.
                if previous_resolutions != Some(&acquisition.resolutions) {
                    w.resolution.set_model(Some(&gtk::StringList::new(&labels)));
                }
                let index = acquisition
                    .resolutions
                    .iter()
                    .position(|r| *r == acquisition.resolution);
                w.resolution
                    .set_selected(index.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32));
                w.paper_label.set_visible(acquisition.paper.is_some());
                w.paper.set_visible(acquisition.paper.is_some());
                if let Some(paper) = acquisition.paper {
                    let index = PaperSize::ALL.iter().position(|p| *p == paper).unwrap_or(0);
                    w.paper.set_selected(index as u32);
                }
            }
            None => w.scan_section.set_visible(false),
        }

        if previous_modes != Some(&state.allowed_modes) {
            let labels: Vec<&str> = state.allowed_modes.iter().map(|m| m.label()).collect();
            w.mode.set_model(Some(&gtk::StringList::new(&labels)));
        }
        let index = state
            .allowed_modes
            .iter()
            .position(|m| *m == a.mode)
            .unwrap_or(0);
        w.mode.set_selected(index as u32);

        w.straighten_label.set_visible(state.can_straighten);
        w.straighten_box.set_visible(state.can_straighten);
        w.straighten.set_value(f64::from(a.straighten));

        w.crop_label.set_text(&match state.crop_size_mm {
            Some((width, height)) => format!("{width:.0} × {height:.0} mm"),
            None => "Whole area".to_owned(),
        });

        w.brightness.set_value(f64::from(a.brightness) * 100.0);
        w.contrast.set_value(f64::from(a.contrast) * 100.0);
        w.sharpen.set_value(f64::from(a.sharpen) * 100.0);

        let black_and_white = a.mode == OutputMode::BlackAndWhite;
        w.threshold_label.set_visible(black_and_white);
        w.threshold.set_visible(black_and_white);
        w.auto_threshold.set_visible(black_and_white);
        match a.threshold {
            Threshold::Manual(level) => {
                w.auto_threshold.set_active(false);
                w.threshold.set_sensitive(true);
                w.threshold.set_value(f64::from(level));
            }
            Threshold::Auto => {
                w.auto_threshold.set_active(true);
                w.threshold.set_sensitive(false);
                if let Some(level) = state.auto_threshold {
                    w.threshold.set_value(f64::from(level));
                }
            }
        }

        *self.state.borrow_mut() = Some(state.clone());
        *self.displayed.borrow_mut() = Some(state);
        self.updating.set(false);
    }

    fn connect_signals(&self) {
        let w = &self.widgets;

        let this = self.clone();
        w.resolution.connect_selected_notify(move |dropdown| {
            let resolution = this.with_state(|s| {
                s.acquisition
                    .as_ref()
                    .and_then(|a| a.resolutions.get(dropdown.selected() as usize).copied())
            });
            if let Some(resolution) = resolution.flatten() {
                this.emit(PanelEvent::Resolution(resolution));
            }
        });

        let this = self.clone();
        w.paper.connect_selected_notify(move |dropdown| {
            if let Some(paper) = PaperSize::ALL.get(dropdown.selected() as usize) {
                this.emit(PanelEvent::Paper(*paper));
            }
        });

        let this = self.clone();
        w.mode.connect_selected_notify(move |dropdown| {
            let mode =
                this.with_state(|s| s.allowed_modes.get(dropdown.selected() as usize).copied());
            if let Some(mode) = mode.flatten() {
                this.update_adjustments(|a| a.mode = mode);
            }
        });

        let this = self.clone();
        w.straighten.connect_value_changed(move |scale| {
            let degrees = (scale.value() * 10.0).round() as f32 / 10.0;
            this.update_adjustments(|a| a.straighten = degrees);
        });

        let scale_fields: [(&gtk::Scale, ScaleSetter); 3] = [
            (&w.brightness, |a, v| a.brightness = v / 100.0),
            (&w.contrast, |a, v| a.contrast = v / 100.0),
            (&w.sharpen, |a, v| a.sharpen = v / 100.0),
        ];
        for (scale, apply) in scale_fields {
            let this = self.clone();
            scale.connect_value_changed(move |scale| {
                let value = scale.value() as f32;
                this.update_adjustments(|a| apply(a, value));
            });
        }

        let this = self.clone();
        w.threshold.connect_value_changed(move |scale| {
            let level = scale.value().round().clamp(0.0, 255.0) as u8;
            this.update_adjustments(|a| {
                if matches!(a.threshold, Threshold::Manual(_)) {
                    a.threshold = Threshold::Manual(level);
                }
            });
        });

        let this = self.clone();
        w.auto_threshold.connect_toggled(move |check| {
            let auto = check.is_active();
            // Switching to manual starts from the level last used.
            let level = this.widgets.threshold.value().round().clamp(0.0, 255.0) as u8;
            this.widgets.threshold.set_sensitive(!auto);
            this.update_adjustments(|a| {
                a.threshold = if auto {
                    Threshold::Auto
                } else {
                    Threshold::Manual(level)
                };
            });
        });
    }

    fn with_state<T>(&self, f: impl FnOnce(&PanelState) -> T) -> Option<T> {
        self.state.borrow().as_ref().map(f)
    }

    fn update_adjustments(&self, change: impl FnOnce(&mut Adjustments)) {
        if self.updating.get() {
            return;
        }
        let adjustments = {
            let mut state = self.state.borrow_mut();
            let Some(state) = state.as_mut() else { return };
            change(&mut state.adjustments);
            state.adjustments
        };
        self.emit(PanelEvent::Adjustments(adjustments));
    }

    fn emit(&self, event: PanelEvent) {
        if self.updating.get() {
            return;
        }
        if let Some(callback) = &*self.on_event.borrow() {
            callback(event);
        }
    }
}

fn section(title: &str, content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let heading = gtk::Label::builder().label(title).xalign(0.0).build();
    heading.add_css_class("heading");
    let section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    section.append(&heading);
    section.append(content);
    section
}

fn grid() -> gtk::Grid {
    gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build()
}

fn field_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).build()
}

fn attach_row(grid: &gtk::Grid, row: i32, label: &gtk::Label, control: &impl IsA<gtk::Widget>) {
    control.set_hexpand(true);
    grid.attach(label, 0, row, 1, 1);
    grid.attach(control, 1, row, 1, 1);
}

fn slider(min: f64, max: f64, mark: f64) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, 1.0);
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.add_mark(mark, gtk::PositionType::Bottom, None);
    scale.set_width_request(160);
    scale
}
