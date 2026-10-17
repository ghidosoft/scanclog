//! The main window: wiring between scanner, document, and widgets.

use super::canvas::{Canvas, Viewport};
use super::dialogs::{self, UnsavedChoice};
use super::jobs::{self, JobError, JobEvent};
use super::page_list::{ListSelection, PageList, PageSummary};
use super::panel::{Acquisition, Panel, PanelEvent, PanelState};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use scanclog::config::{Config, SavedScanner};
use scanclog::escl::{
    Capabilities, Client, Endpoint, InputSource, Job, ScannerInfo, UNITS_PER_INCH,
};
use scanclog::export::{self, FileFormat, PdfOptions};
use scanclog::imaging::{self, Adjustments, Bitmap, Crop, OutputMode, Rotation, Target};
use scanclog::model::{Document, PageId, ScannedImage};
use scanclog::scanning::{self, ScanArea, ScanRequest};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Longest side of the downscaled copy of a page used for on-screen editing.
const PROXY_SIZE: u32 = 2000;
/// How many downscaled pages to keep in memory.
const PROXY_CACHE: usize = 4;
/// Detected tilts below this (degrees) are not worth straightening
/// automatically: the difference is invisible.
const MIN_AUTO_STRAIGHTEN: f32 = 0.3;
/// Thumbnail size in device pixels (logical size × 2 for HiDPI screens).
const THUMBNAIL_PIXELS: u32 = 280;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    Preview,
    Page(PageId),
}

enum Connection {
    None,
    Connecting(usize),
    Connected {
        index: usize,
        client: Client,
        caps: Box<Capabilities>,
    },
    Failed(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Preview,
    Scan(InputSource),
}

struct Busy {
    /// Adjustments for the pages this job creates.
    page_adjustments: Adjustments,
    job: Option<Job>,
    cancel_requested: bool,
    source: InputSource,
    resolution: u32,
    /// Scanned height in 1/300 in, for timing.
    height: u32,
    /// When the page now being scanned started.
    page_started: Instant,
    /// How long one page is expected to take.
    expected: Duration,
}

struct State {
    config: Config,
    scanners: Vec<ScannerInfo>,
    discovering: bool,
    connection: Connection,
    source: InputSource,
    preview: Option<Arc<Bitmap>>,
    document: Document,
    selection: Selection,
    /// Downscaled pages for editing, most recently used last.
    proxies: VecDeque<(PageId, Arc<Bitmap>)>,
    /// The full-resolution decoded image of one page, for zooming in.
    full_image: Option<(PageId, Arc<Bitmap>)>,
    thumbnails: HashMap<PageId, gdk::Texture>,
    thumbnail_queue: VecDeque<PageId>,
    thumbnail_running: bool,
    render_running: bool,
    render_pending: bool,
    auto_threshold: Option<u8>,
    busy: Option<Busy>,
    saving: bool,
    status: String,
}

impl State {
    fn caps(&self) -> Option<&Capabilities> {
        match &self.connection {
            Connection::Connected { caps, .. } => Some(caps),
            _ => None,
        }
    }

    fn selected_adjustments(&self) -> Adjustments {
        match self.selection {
            Selection::Preview => self.config.template,
            Selection::Page(id) => self
                .document
                .get(id)
                .map_or(self.config.template, |p| p.adjustments),
        }
    }

    fn page_index(&self) -> Option<usize> {
        match self.selection {
            Selection::Page(id) => self.document.index_of(id),
            Selection::Preview => None,
        }
    }

    fn proxy(&mut self, id: PageId) -> Option<Arc<Bitmap>> {
        let position = self.proxies.iter().position(|(p, _)| *p == id)?;
        let entry = self.proxies.remove(position)?;
        let bitmap = entry.1.clone();
        self.proxies.push_back(entry);
        Some(bitmap)
    }

    fn store_proxy(&mut self, id: PageId, bitmap: Arc<Bitmap>) {
        self.proxies.retain(|(p, _)| *p != id);
        self.proxies.push_back((id, bitmap));
        while self.proxies.len() > PROXY_CACHE {
            self.proxies.pop_front();
        }
    }
}

/// What the canvas should show.
enum Display {
    Message(&'static str),
    NeedsProxy(PageId, Arc<ScannedImage>),
    Ready {
        selection: Selection,
        /// The preview, or the downscaled copy of a page.
        source: Arc<Bitmap>,
        adjustments: Adjustments,
        /// For pages: the original, and its decoded form if cached, for
        /// rendering detail beyond what `source` holds.
        full: Option<(PageId, Arc<ScannedImage>, Option<Arc<Bitmap>>)>,
        /// Width of the full-resolution image, in its own frame.
        full_width: u32,
    },
}

pub struct App {
    window: gtk::ApplicationWindow,
    scanner_dropdown: gtk::DropDown,
    refresh_button: gtk::Button,
    source_dropdown: gtk::DropDown,
    source_options: RefCell<Vec<InputSource>>,
    canvas: Canvas,
    pages: PageList,
    panel: Panel,
    status_label: gtk::Label,
    spinner: gtk::Spinner,
    actions: RefCell<HashMap<&'static str, gio::SimpleAction>>,
    /// Set while toolbar widgets are updated from code.
    updating_toolbar: Cell<bool>,
    /// Set once the user agreed to close despite unsaved pages.
    close_confirmed: Cell<bool>,
    /// Set when the user closed the window while a save was running.
    close_after_save: Cell<bool>,
    state: RefCell<State>,
}

impl App {
    pub fn new(application: &gtk::Application) -> Rc<Self> {
        let config = Config::load();
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("ScanClog")
            .default_width(1200)
            .default_height(820)
            .build();

        let scanner_dropdown = gtk::DropDown::from_strings(&[]);
        scanner_dropdown.set_tooltip_text(Some("Scanner"));
        let refresh_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Search for scanners again")
            .build();
        let source_dropdown = gtk::DropDown::from_strings(&[]);
        source_dropdown.set_tooltip_text(Some("Paper source"));
        let save_button = gtk::Button::builder()
            .label("Save PDF")
            .action_name("win.save")
            .tooltip_text("Save all pages as one PDF (Ctrl+S)")
            .build();
        save_button.add_css_class("suggested-action");
        let menu = gio::Menu::new();
        let scanner_section = gio::Menu::new();
        scanner_section.append(Some("Add Scanner by Address…"), Some("win.add-scanner"));
        menu.append_section(None, &scanner_section);
        let page_section = gio::Menu::new();
        page_section.append(Some("Export Selected Page…"), Some("win.export-page"));
        page_section.append(
            Some("Apply Settings to All Pages"),
            Some("win.apply-to-all"),
        );
        page_section.append(Some("Remove All Pages…"), Some("win.clear"));
        menu.append_section(None, &page_section);
        let app_section = gio::Menu::new();
        app_section.append(Some("About ScanClog"), Some("win.about"));
        menu.append_section(None, &app_section);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Menu")
            .build();

        let header = gtk::HeaderBar::new();
        header.pack_start(&scanner_dropdown);
        header.pack_start(&refresh_button);
        header.pack_start(&source_dropdown);
        header.pack_end(&menu_button);
        header.pack_end(&save_button);
        window.set_titlebar(Some(&header));

        let canvas = Canvas::new();
        canvas.widget().add_css_class("scanclog-canvas");
        let pages = PageList::new();
        let panel = Panel::new();

        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let spinner = gtk::Spinner::new();
        let status_bar = gtk::Box::builder()
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(12)
            .margin_end(12)
            .build();
        status_bar.append(&spinner);
        status_bar.append(&status_label);

        let center = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(canvas.widget())
            .end_child(panel.widget())
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_end_child(false)
            .build();
        let main = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(pages.widget())
            .end_child(&center)
            .resize_start_child(false)
            .shrink_start_child(false)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main.set_vexpand(true);
        content.append(&main);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&status_bar);
        window.set_child(Some(&content));

        let source = config.source;
        let app = Rc::new(App {
            window,
            scanner_dropdown,
            refresh_button,
            source_dropdown,
            source_options: RefCell::default(),
            canvas,
            pages,
            panel,
            status_label,
            spinner,
            actions: RefCell::default(),
            updating_toolbar: Cell::new(false),
            close_confirmed: Cell::new(false),
            close_after_save: Cell::new(false),
            state: RefCell::new(State {
                config,
                scanners: Vec::new(),
                discovering: false,
                connection: Connection::None,
                source,
                preview: None,
                document: Document::new(),
                selection: Selection::Preview,
                proxies: VecDeque::new(),
                full_image: None,
                thumbnails: HashMap::new(),
                thumbnail_queue: VecDeque::new(),
                thumbnail_running: false,
                render_running: false,
                render_pending: false,
                auto_threshold: None,
                busy: None,
                saving: false,
                status: String::new(),
            }),
        });
        app.setup_actions(application);
        app.connect_signals();
        app.add_manual_scanners();
        app.refresh_all();
        app.discover();
        app
    }

    pub fn present(&self) {
        self.window.present();
    }

    // ---------------------------------------------------------------- setup

    fn setup_actions(self: &Rc<Self>, application: &gtk::Application) {
        type Handler = fn(&Rc<App>);
        let actions: [(&'static str, Handler, &[&str]); 24] = [
            ("preview", |app| app.start_preview(), &["<Ctrl>p"]),
            ("scan", |app| app.start_scan(), &["<Ctrl>Return"]),
            ("save", |app| app.spawn_save(), &["<Ctrl>s"]),
            ("export-page", |app| app.spawn_export_page(), &["<Ctrl>e"]),
            ("delete-page", |app| app.delete_page(), &["Delete"]),
            (
                "move-page-up",
                |app| app.move_selected_page(-1),
                &["<Alt>Up"],
            ),
            (
                "move-page-down",
                |app| app.move_selected_page(1),
                &["<Alt>Down"],
            ),
            ("rotate-left", |app| app.rotate(false), &["<Ctrl>l"]),
            ("rotate-right", |app| app.rotate(true), &["<Ctrl>r"]),
            (
                "reset-crop",
                |app| app.edit_adjustments(|a| a.crop = Crop::FULL),
                &[],
            ),
            ("reset-adjustments", |app| app.reset_adjustments(), &[]),
            ("apply-to-all", |app| app.apply_to_all(), &[]),
            ("auto-straighten", |app| app.auto_straighten(), &[]),
            ("detect-crop", |app| app.detect_crop(false), &[]),
            ("cancel", |app| app.cancel_job(), &["Escape"]),
            (
                "zoom-in",
                |app| app.canvas.zoom_in(),
                &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"],
            ),
            (
                "zoom-out",
                |app| app.canvas.zoom_out(),
                &["<Ctrl>minus", "<Ctrl>KP_Subtract"],
            ),
            (
                "zoom-fit",
                |app| app.canvas.zoom_fit(),
                &["<Ctrl>0", "<Ctrl>KP_0"],
            ),
            (
                "zoom-actual",
                |app| app.canvas.zoom_actual(),
                &["<Ctrl>1", "<Ctrl>KP_1"],
            ),
            ("clear", |app| app.spawn_clear(), &[]),
            ("add-scanner", |app| app.spawn_add_scanner(), &[]),
            (
                "about",
                |app| dialogs::show_about(app.window.upcast_ref()),
                &[],
            ),
            ("refresh", |app| app.discover(), &["F5"]),
            ("quit", |app| app.window.close(), &["<Ctrl>q"]),
        ];
        for (name, handler, accels) in actions {
            let action = gio::SimpleAction::new(name, None);
            let app = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(app) = app.upgrade() {
                    handler(&app);
                }
            });
            self.window.add_action(&action);
            if !accels.is_empty() {
                application.set_accels_for_action(&format!("win.{name}"), accels);
            }
            self.actions.borrow_mut().insert(name, action);
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.refresh_button.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                app.discover();
            }
        });

        let app = Rc::downgrade(self);
        self.scanner_dropdown
            .connect_selected_notify(move |dropdown| {
                let Some(app) = app.upgrade() else { return };
                if !app.updating_toolbar.get() {
                    // Defer: the handlers rebuild the toolbar, which GTK does
                    // not allow while the dropdown is still processing the click.
                    let index = dropdown.selected() as usize;
                    glib::idle_add_local_once(move || app.connect_scanner(index));
                }
            });

        let app = Rc::downgrade(self);
        self.source_dropdown
            .connect_selected_notify(move |dropdown| {
                let Some(app) = app.upgrade() else { return };
                if app.updating_toolbar.get() {
                    return;
                }
                let source = app
                    .source_options
                    .borrow()
                    .get(dropdown.selected() as usize)
                    .copied();
                if let Some(source) = source {
                    glib::idle_add_local_once(move || app.set_source(source));
                }
            });

        let app = Rc::downgrade(self);
        self.pages.connect_selected(move |selection| {
            let Some(app) = app.upgrade() else { return };
            let selection = {
                let state = app.state.borrow();
                match selection {
                    ListSelection::Preview => Selection::Preview,
                    ListSelection::Page(index) => match state.document.pages().get(index) {
                        Some(page) => Selection::Page(page.id),
                        None => Selection::Preview,
                    },
                }
            };
            app.select(selection);
        });

        let app = Rc::downgrade(self);
        self.pages.connect_moved(move |from, to| {
            let Some(app) = app.upgrade() else { return };
            let moved = {
                let mut state = app.state.borrow_mut();
                let id = state.document.pages().get(from).map(|p| p.id);
                id.is_some_and(|id| state.document.move_to(id, to))
            };
            if moved {
                app.refresh_page_list();
                app.update_actions();
            }
        });

        let app = Rc::downgrade(self);
        self.panel.connect_event(move |event| {
            let Some(app) = app.upgrade() else { return };
            match event {
                PanelEvent::Adjustments(adjustments) => app.edit_adjustments(|a| *a = adjustments),
                PanelEvent::Resolution(resolution) => {
                    app.state.borrow_mut().config.resolution = resolution
                }
                PanelEvent::Paper(paper) => app.state.borrow_mut().config.feeder_paper = paper,
            }
        });

        let app = Rc::downgrade(self);
        self.canvas.connect_crop_changed(move |crop| {
            if let Some(app) = app.upgrade() {
                app.edit_adjustments(|a| a.crop = crop);
            }
        });

        let app = Rc::downgrade(self);
        self.canvas.connect_view_changed(move || {
            if let Some(app) = app.upgrade() {
                app.request_render();
            }
        });

        // This handler holds the only strong reference to the app: it lives
        // exactly as long as the window, and every other callback uses a weak
        // reference to avoid cycles.
        let app = self.clone();
        self.window
            .connect_close_request(move |_| app.on_close_request());
    }

    fn add_manual_scanners(&self) {
        let mut state = self.state.borrow_mut();
        let addresses = state.config.manual_scanners.clone();
        for address in addresses {
            match Endpoint::parse(&address) {
                Ok(endpoint) => state.scanners.push(ScannerInfo::manual(endpoint)),
                Err(err) => log::warn!("ignoring saved scanner '{address}': {err}"),
            }
        }
    }

    // ------------------------------------------------------------ scanners

    fn discover(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if state.discovering {
                return;
            }
            state.discovering = true;
        }
        self.set_status("Searching for scanners…");
        let receiver = jobs::discover();
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(found) = receiver.recv().await {
                let Some(app) = app.upgrade() else { return };
                app.merge_scanners(found);
            }
            let Some(app) = app.upgrade() else { return };
            let (none, auto_connect) = {
                let mut state = app.state.borrow_mut();
                state.discovering = false;
                (
                    state.scanners.is_empty(),
                    auto_connect_candidate(&state, true),
                )
            };
            if let Some(index) = auto_connect {
                app.connect_scanner(index);
            }
            if none {
                app.set_status("No scanner found. Use “Add Scanner by Address…” in the menu if yours is not listed.");
            } else if matches!(app.state.borrow().connection, Connection::None) {
                app.set_status("Choose a scanner.");
            }
            app.update_toolbar();
            app.update_status();
        });
    }

    fn merge_scanners(self: &Rc<Self>, found: Vec<ScannerInfo>) {
        log::debug!("discovered {} scanner(s)", found.len());
        let auto_connect = {
            let mut state = self.state.borrow_mut();
            for scanner in found {
                let existing = state.scanners.iter_mut().find(|s| {
                    (s.uuid.is_some() && s.uuid == scanner.uuid)
                        || s.endpoints.iter().any(|e| scanner.endpoints.contains(e))
                });
                match existing {
                    Some(existing) => {
                        for endpoint in scanner.endpoints {
                            if !existing.endpoints.contains(&endpoint) {
                                existing.endpoints.push(endpoint);
                            }
                        }
                        existing.endpoints.sort_by_key(|e| e.tls);
                        if existing.uuid.is_none() {
                            existing.name = scanner.name;
                            existing.uuid = scanner.uuid;
                        }
                    }
                    None => state.scanners.push(scanner),
                }
            }
            auto_connect_candidate(&state, false)
        };
        self.update_toolbar();
        if let Some(index) = auto_connect {
            self.connect_scanner(index);
        }
    }

    fn connect_scanner(self: &Rc<Self>, index: usize) {
        let info = {
            let mut state = self.state.borrow_mut();
            if state.busy.is_some() {
                // The dropdown changed although it should not have: put it back.
                drop(state);
                let app = Rc::downgrade(self);
                glib::idle_add_local_once(move || {
                    if let Some(app) = app.upgrade() {
                        app.update_toolbar();
                    }
                });
                return;
            }
            let Some(info) = state.scanners.get(index).cloned() else {
                return;
            };
            match state.connection {
                Connection::Connecting(i) | Connection::Connected { index: i, .. }
                    if i == index =>
                {
                    return;
                }
                _ => {}
            }
            state.connection = Connection::Connecting(index);
            state.preview = None;
            info
        };
        self.set_status(&format!("Connecting to {}…", info.name));
        self.refresh_all();

        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let name = info.name.clone();
            let uuid = info.uuid.clone();
            let result = jobs::connect(info).await;
            let Some(app) = app.upgrade() else { return };
            {
                let mut state = app.state.borrow_mut();
                // The user may have picked another scanner meanwhile.
                if !matches!(state.connection, Connection::Connecting(i) if i == index) {
                    return;
                }
                match result {
                    Ok((client, caps)) => {
                        // Scanners added by address are listed under their
                        // host until the device tells its model.
                        let name = match (&uuid, &caps.make_and_model) {
                            (None, Some(model)) => {
                                state.scanners[index].name = model.clone();
                                model.clone()
                            }
                            _ => name,
                        };
                        state.config.scanner = Some(SavedScanner {
                            name: name.clone(),
                            uuid,
                            endpoint: client.endpoint().base_url(),
                        });
                        if caps.source(state.source).is_none() {
                            state.source = caps.sources()[0];
                        }
                        state.connection = Connection::Connected {
                            index,
                            client,
                            caps: Box::new(caps),
                        };
                        state.status = format!("Connected to {name}.");
                    }
                    Err(err) => {
                        state.connection = Connection::Failed(index);
                        state.status = format!("Cannot connect to {name}: {err}");
                    }
                }
            }
            app.refresh_all();
        });
    }

    fn spawn_add_scanner(self: &Rc<Self>) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            let Some(endpoint) = dialogs::ask_scanner_address(app.window.upcast_ref()).await else {
                return;
            };
            let index = {
                let mut state = app.state.borrow_mut();
                let address = endpoint.base_url();
                if !state.config.manual_scanners.contains(&address) {
                    state.config.manual_scanners.push(address);
                }
                match state
                    .scanners
                    .iter()
                    .position(|s| s.endpoints.contains(&endpoint))
                {
                    Some(index) => index,
                    None => {
                        state.scanners.push(ScannerInfo::manual(endpoint));
                        state.scanners.len() - 1
                    }
                }
            };
            app.update_toolbar();
            app.connect_scanner(index);
        });
    }

    fn set_source(self: &Rc<Self>, source: InputSource) {
        {
            let mut state = self.state.borrow_mut();
            if state.source == source {
                return;
            }
            state.source = source;
            state.config.source = source;
        }
        self.refresh_all();
    }

    // ------------------------------------------------------------ scanning

    fn start_preview(self: &Rc<Self>) {
        let prepared = {
            let state = self.state.borrow();
            match &state.connection {
                Connection::Connected { client, caps, .. } if state.busy.is_none() => {
                    scanning::preview_settings(caps).map(|settings| (client.clone(), settings))
                }
                _ => return,
            }
        };
        match prepared {
            Ok((client, settings)) => {
                self.run_job(JobKind::Preview, client, settings, Adjustments::default())
            }
            Err(err) => self.show_error("Cannot preview", &err.to_string()),
        }
    }

    fn start_scan(self: &Rc<Self>) {
        let prepared = {
            let state = self.state.borrow();
            let Connection::Connected { client, caps, .. } = &state.connection else {
                return;
            };
            if state.busy.is_some() {
                return;
            }
            let template = state.config.template;
            let platen = caps.platen.as_ref();
            // What the new pages start with; the crop depends on what the
            // scanner already cut away.
            let (area, page_adjustments) = match (state.source, platen) {
                (InputSource::Platen, Some(bed))
                    if !template.crop.is_full() || template.straighten != 0.0 =>
                {
                    let plan = scanning::plan_region(
                        template.crop,
                        template.rotation,
                        template.straighten,
                        bed.max_width,
                        bed.max_height,
                    );
                    let page = Adjustments {
                        crop: plan.page_crop,
                        ..template
                    };
                    (ScanArea::Region(plan.region), page)
                }
                (InputSource::Platen, _) => (
                    ScanArea::Full,
                    Adjustments {
                        crop: Crop::FULL,
                        ..template
                    },
                ),
                // Without a preview there is nothing to crop or straighten.
                (InputSource::Feeder, _) => (
                    ScanArea::Paper(state.config.feeder_paper),
                    Adjustments {
                        crop: Crop::FULL,
                        straighten: 0.0,
                        ..template
                    },
                ),
            };
            let request = ScanRequest {
                source: state.source,
                resolution: state.config.resolution,
                area,
            };
            scanning::scan_settings(caps, &request)
                .map(|settings| (client.clone(), settings, state.source, page_adjustments))
        };
        match prepared {
            Ok((client, settings, source, page)) => {
                self.run_job(JobKind::Scan(source), client, settings, page)
            }
            Err(err) => self.show_error("Cannot scan", &err.to_string()),
        }
    }

    fn run_job(
        self: &Rc<Self>,
        kind: JobKind,
        client: Client,
        settings: scanclog::escl::ScanSettings,
        page_adjustments: Adjustments,
    ) {
        {
            let mut state = self.state.borrow_mut();
            let height = settings.region.map_or(0, |r| r.height);
            let expected =
                state
                    .config
                    .scan_timing
                    .estimate(settings.source, settings.resolution, height);
            state.busy = Some(Busy {
                page_adjustments,
                job: None,
                cancel_requested: false,
                source: settings.source,
                resolution: settings.resolution,
                height,
                page_started: Instant::now(),
                expected,
            });
        }
        self.start_progress_timer();
        self.set_status(match kind {
            JobKind::Preview => "Scanning preview…",
            JobKind::Scan(InputSource::Platen) => "Scanning…",
            JobKind::Scan(InputSource::Feeder) => "Scanning from the document feeder…",
        });
        self.update_actions();

        let receiver = jobs::run_scan(client, settings, kind == JobKind::Preview);
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // Tracked by id: pages may be moved or deleted during the scan.
            let mut first_new_page = None;
            while let Ok(event) = receiver.recv().await {
                let Some(app) = app.upgrade() else { return };
                match event {
                    JobEvent::Started(job) => app.job_started(job),
                    JobEvent::Page { image, decoded } => {
                        app.page_received();
                        let id = app.job_page(kind, image, decoded);
                        first_new_page = first_new_page.or(id);
                    }
                    JobEvent::Finished(result) => app.job_finished(kind, result, first_new_page),
                }
            }
        });
    }

    /// Advances the progress bar while a job runs.
    fn start_progress_timer(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let Some(app) = app.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let fraction =
                app.state.borrow().busy.as_ref().map(|busy| {
                    scanclog::timing::progress(busy.page_started.elapsed(), busy.expected)
                });
            match fraction {
                Some(fraction) => {
                    let text = format!("{:.0} %", fraction * 100.0);
                    app.panel.set_progress(Some((fraction, &text)));
                    glib::ControlFlow::Continue
                }
                None => {
                    app.panel.set_progress(None);
                    glib::ControlFlow::Break
                }
            }
        });
    }

    /// Learns from a completed page and restarts the estimate for the next one.
    fn page_received(&self) {
        let mut state = self.state.borrow_mut();
        let State { busy, config, .. } = &mut *state;
        let Some(busy) = busy.as_mut() else { return };
        config.scan_timing.record(
            busy.source,
            busy.resolution,
            busy.height,
            busy.page_started.elapsed(),
        );
        busy.page_started = Instant::now();
        busy.expected = config
            .scan_timing
            .estimate(busy.source, busy.resolution, busy.height);
    }

    fn job_started(&self, job: Job) {
        let mut state = self.state.borrow_mut();
        let Some(busy) = state.busy.as_mut() else {
            return;
        };
        if busy.cancel_requested {
            jobs::cancel(job.clone());
        }
        busy.job = Some(job);
    }

    /// Handles one received image; returns the id of the page it created.
    fn job_page(
        self: &Rc<Self>,
        kind: JobKind,
        image: ScannedImage,
        decoded: Option<Bitmap>,
    ) -> Option<PageId> {
        match kind {
            JobKind::Preview => {
                let bitmap = decoded?;
                {
                    let mut state = self.state.borrow_mut();
                    state.preview = Some(Arc::new(bitmap));
                    state.selection = Selection::Preview;
                }
                self.pages.select(ListSelection::Preview);
                self.refresh_selection();
                // Propose a crop, unless the user already drew one.
                if self.state.borrow().config.template.crop.is_full() {
                    self.detect_crop(true);
                }
                None
            }
            JobKind::Scan(_) => {
                let (id, count) = {
                    let mut state = self.state.borrow_mut();
                    let adjustments = state.busy.as_ref().map(|b| b.page_adjustments)?;
                    let id = state.document.push(image, adjustments);
                    let count = state.document.len();
                    state.status = format!("Added page {count}.");
                    (id, count)
                };
                log::debug!("page {count} scanned");
                self.refresh_page_list();
                self.queue_thumbnail(id);
                self.update_status();
                self.update_actions();
                Some(id)
            }
        }
    }

    fn job_finished(
        self: &Rc<Self>,
        kind: JobKind,
        result: Result<usize, JobError>,
        first_new_page: Option<PageId>,
    ) {
        self.state.borrow_mut().busy = None;
        match result {
            Ok(0) => self.show_error(
                "The scanner returned no image",
                "Check that the document is in place and try again.",
            ),
            Ok(count) => {
                if let JobKind::Scan(InputSource::Feeder) = kind {
                    // There is no preview for the feeder: show the first new page.
                    let exists =
                        first_new_page.filter(|id| self.state.borrow().document.get(*id).is_some());
                    if let Some(id) = exists {
                        self.select(Selection::Page(id));
                    }
                }
                self.set_status(&match kind {
                    JobKind::Preview => {
                        "Preview ready. Drag on the image to select the area to scan.".to_owned()
                    }
                    JobKind::Scan(_) if count == 1 => {
                        format!("Added page {}.", self.state.borrow().document.len())
                    }
                    JobKind::Scan(_) => format!("Added {count} pages."),
                });
            }
            Err(JobError::Cancelled) => self.set_status("Scan cancelled."),
            Err(JobError::Failed(message)) => {
                self.set_status("Scan failed.");
                self.show_error("Scan failed", &dialogs::capitalize(&message));
            }
        }
        self.update_actions();
    }

    fn cancel_job(&self) {
        let job = {
            let mut state = self.state.borrow_mut();
            let Some(busy) = state.busy.as_mut() else {
                return;
            };
            busy.cancel_requested = true;
            busy.job.clone()
        };
        if let Some(job) = job {
            jobs::cancel(job);
        }
        self.set_status("Cancelling…");
    }

    // ------------------------------------------------------------- editing

    fn select(self: &Rc<Self>, selection: Selection) {
        {
            let mut state = self.state.borrow_mut();
            if state.selection == selection {
                return;
            }
            state.selection = selection;
            state.auto_threshold = None;
        }
        self.pages.select(match selection {
            Selection::Preview => ListSelection::Preview,
            Selection::Page(id) => {
                ListSelection::Page(self.state.borrow().document.index_of(id).unwrap_or(0))
            }
        });
        self.refresh_selection();
    }

    /// Changes the adjustments of the selected entry (template or page).
    fn edit_adjustments(self: &Rc<Self>, change: impl FnOnce(&mut Adjustments)) {
        let (before, after, page) = {
            let mut state = self.state.borrow_mut();
            let before = state.selected_adjustments();
            let mut after = before;
            change(&mut after);
            if after == before {
                return;
            }
            let page = match state.selection {
                Selection::Preview => {
                    state.config.template = after;
                    None
                }
                Selection::Page(id) => {
                    state.document.set_adjustments(id, after);
                    Some(id)
                }
            };
            (before, after, page)
        };
        // Crops drawn on the canvas are already shown; this covers changes
        // made elsewhere (reset, rotation).
        if before.rotation != after.rotation {
            let size = content_size(&self.state.borrow());
            if let Some((width, height, _)) = size {
                self.canvas.set_content_size(width, height);
            }
        }
        if (before.crop != after.crop || before.rotation != after.rotation)
            && self.canvas.has_crop()
        {
            self.canvas.set_crop(Some(after.crop));
        }
        if let Some(id) = page {
            self.queue_thumbnail(id);
            if before.mode != after.mode {
                self.refresh_page_list();
            }
        }
        self.request_render();
        self.refresh_panel();
        self.update_actions();
    }

    fn rotate(self: &Rc<Self>, clockwise: bool) {
        self.edit_adjustments(|a| {
            let rotation = if clockwise {
                a.rotation.clockwise()
            } else {
                a.rotation.counter_clockwise()
            };
            a.crop = a.crop.to_source(a.rotation).from_source(rotation);
            a.rotation = rotation;
        });
    }

    /// The selected image for analysis: the preview, or a page (its
    /// downscaled copy if cached, otherwise the original), with its
    /// adjustments.
    fn analysis_input(&self) -> Option<(Selection, AnalysisImage, Adjustments)> {
        let mut state = self.state.borrow_mut();
        let selection = state.selection;
        match selection {
            Selection::Preview => {
                let preview = state.preview.clone()?;
                Some((
                    selection,
                    AnalysisImage::Ready(preview),
                    state.config.template,
                ))
            }
            Selection::Page(id) => {
                let page = state.document.get(id)?;
                let (image, adjustments) = (page.image.clone(), page.adjustments);
                let input = match state.proxy(id) {
                    Some(proxy) => AnalysisImage::Ready(proxy),
                    None => AnalysisImage::Original(image),
                };
                Some((selection, input, adjustments))
            }
        }
    }

    /// Finds the document in the selected image and crops to it. When
    /// `automatic` (right after a preview), the tilt is detected first and
    /// failures stay silent.
    fn detect_crop(self: &Rc<Self>, automatic: bool) {
        let Some((selection, input, adjustments)) = self.analysis_input() else {
            return;
        };
        let detect_tilt = automatic && adjustments.straighten == 0.0;
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = jobs::background(move || {
                let source = input.load()?;
                let straighten = if detect_tilt {
                    detect_tilt_of(&source, adjustments.rotation)
                        .filter(|degrees| degrees.abs() >= MIN_AUTO_STRAIGHTEN)
                        .unwrap_or(0.0)
                } else {
                    adjustments.straighten
                };
                // Detect in the frame the user sees: rotated and straightened.
                let view = Adjustments {
                    rotation: adjustments.rotation,
                    straighten,
                    ..Adjustments::default()
                };
                let view = imaging::render(&source, &view, Target::Full);
                Ok::<_, imaging::DecodeError>((straighten, imaging::detect_document(&view)))
            })
            .await;
            let Some(app) = app.upgrade() else { return };
            if app.state.borrow().selection != selection {
                return;
            }
            match found {
                Ok((straighten, Some(crop))) => {
                    app.edit_adjustments(|a| {
                        a.straighten = straighten;
                        a.crop = crop;
                    });
                    app.set_status(&match (automatic, straighten) {
                        (true, 0.0) => {
                            "Preview ready. The document was selected; adjust the area if needed.".to_owned()
                        }
                        (true, degrees) => format!(
                            "Preview ready. The document was selected and straightened by {degrees:.1}°; adjust if needed."
                        ),
                        (false, _) => "Document found.".to_owned(),
                    });
                }
                Ok((_, None)) if !automatic => {
                    app.set_status("No document stands out from the background.");
                }
                Ok((_, None)) => {}
                Err(err) => app.set_status(&format!("Cannot read the page: {err}")),
            }
        });
    }

    /// Detects the tilt of the selected image and straightens it.
    fn auto_straighten(self: &Rc<Self>) {
        let Some((selection, input, adjustments)) = self.analysis_input() else {
            return;
        };
        self.set_status("Detecting the tilt…");
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let detected = jobs::background(move || {
                let source = input.load()?;
                Ok::<_, imaging::DecodeError>(detect_tilt_of(&source, adjustments.rotation))
            })
            .await;
            let Some(app) = app.upgrade() else { return };
            // Only if the user is still looking at the same image.
            if app.state.borrow().selection != selection {
                return;
            }
            match detected {
                Ok(Some(degrees)) => {
                    let degrees = (degrees * 10.0).round() / 10.0;
                    app.edit_adjustments(|a| a.straighten = degrees);
                    app.set_status(&format!("Straightened by {degrees:.1}°."));
                }
                Ok(None) => app.set_status("Not enough content to detect the tilt."),
                Err(err) => app.set_status(&format!("Cannot read the page: {err}")),
            }
        });
    }

    fn reset_adjustments(self: &Rc<Self>) {
        self.edit_adjustments(|a| {
            let defaults = Adjustments::default();
            a.brightness = defaults.brightness;
            a.contrast = defaults.contrast;
            a.sharpen = defaults.sharpen;
            a.threshold = defaults.threshold;
        });
    }

    /// Copies the selected entry's adjustments (except geometry) to every page.
    fn apply_to_all(self: &Rc<Self>) {
        let changed: Vec<PageId> = {
            let mut state = self.state.borrow_mut();
            let source = state.selected_adjustments();
            let pages: Vec<_> = state
                .document
                .pages()
                .iter()
                .map(|p| (p.id, p.adjustments, p.image.clone()))
                .collect();
            pages
                .into_iter()
                .filter_map(|(id, current, image)| {
                    let mode = if image.allowed_modes().contains(&source.mode) {
                        source.mode
                    } else {
                        OutputMode::Gray
                    };
                    let updated = Adjustments {
                        rotation: current.rotation,
                        straighten: current.straighten,
                        crop: current.crop,
                        mode,
                        ..source
                    };
                    state.document.set_adjustments(id, updated).then_some(id)
                })
                .collect()
        };
        for id in &changed {
            self.queue_thumbnail(*id);
        }
        self.refresh_page_list();
        self.refresh_selection();
        self.set_status(&format!("Updated {} pages.", changed.len()));
    }

    fn delete_page(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            let Selection::Page(id) = state.selection else {
                return;
            };
            let Some(index) = state.document.index_of(id) else {
                return;
            };
            state.document.remove(id);
            state.thumbnails.remove(&id);
            state.proxies.retain(|(p, _)| *p != id);
            let pages = state.document.pages();
            let next = pages
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| pages.get(i)));
            state.selection = next.map_or(Selection::Preview, |p| Selection::Page(p.id));
        }
        self.refresh_page_list();
        self.refresh_selection();
    }

    fn move_selected_page(self: &Rc<Self>, offset: isize) {
        let moved = {
            let mut state = self.state.borrow_mut();
            let Selection::Page(id) = state.selection else {
                return;
            };
            let Some(index) = state.document.index_of(id) else {
                return;
            };
            match index.checked_add_signed(offset) {
                Some(target) if target < state.document.len() => state.document.move_to(id, target),
                _ => false,
            }
        };
        if moved {
            self.refresh_page_list();
            self.update_actions();
        }
    }

    fn spawn_clear(self: &Rc<Self>) {
        if self.state.borrow().saving {
            self.set_status("Wait for the PDF to be saved before removing pages.");
            return;
        }
        let app = self.clone();
        glib::spawn_future_local(async move {
            if !app.confirm_discard("Remove Pages").await {
                return;
            }
            {
                let mut state = app.state.borrow_mut();
                state.document.clear();
                state.thumbnails.clear();
                state.proxies.clear();
                state.selection = Selection::Preview;
            }
            app.refresh_page_list();
            app.refresh_selection();
            app.set_status("All pages removed.");
        });
    }

    /// Asks what to do with unsaved pages. Returns true if it is fine to
    /// discard them now.
    async fn confirm_discard(self: &Rc<Self>, action: &str) -> bool {
        let pages = {
            let state = self.state.borrow();
            if !state.document.is_modified() {
                return true;
            }
            state.document.len()
        };
        match dialogs::confirm_unsaved(self.window.upcast_ref(), pages, action).await {
            UnsavedChoice::Cancel => false,
            UnsavedChoice::Discard => true,
            UnsavedChoice::Save => self.save_pdf().await,
        }
    }

    fn on_close_request(self: &Rc<Self>) -> glib::Propagation {
        let (modified, saving) = {
            let state = self.state.borrow();
            (state.document.is_modified(), state.saving)
        };
        if saving {
            // Closing now would kill the save; close once it is done.
            self.close_after_save.set(true);
            self.set_status("Closing when the PDF has been saved…");
            return glib::Propagation::Stop;
        }
        // A scan still adding pages would make any answer below stale.
        self.cancel_job();
        if self.close_confirmed.get() || !modified {
            self.shutdown();
            return glib::Propagation::Proceed;
        }
        let app = self.clone();
        glib::spawn_future_local(async move {
            if app.confirm_discard("Close Without Saving").await {
                app.close_confirmed.set(true);
                app.window.close();
            }
        });
        glib::Propagation::Stop
    }

    fn shutdown(&self) {
        // The process exits right after this, so wait (briefly) for the
        // scanner to acknowledge the cancellation; otherwise it may stay busy.
        let job = self
            .state
            .borrow()
            .busy
            .as_ref()
            .and_then(|b| b.job.clone());
        if let Some(job) = job {
            jobs::cancel_and_wait(job, std::time::Duration::from_secs(2));
        }
        let state = self.state.borrow();
        if let Err(err) = state.config.save() {
            log::warn!("cannot save settings: {err}");
        }
    }

    // -------------------------------------------------------------- saving

    fn spawn_save(self: &Rc<Self>) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            app.save_pdf().await;
        });
    }

    /// Asks for a file name and saves all pages as one PDF. Returns true on success.
    async fn save_pdf(self: &Rc<Self>) -> bool {
        let (folder, page_count) = {
            let state = self.state.borrow();
            if state.document.is_empty() || state.saving {
                return false;
            }
            (state.config.last_directory.clone(), state.document.len())
        };
        let name = format!("scan-{}.pdf", today());
        let formats = [FileFormat::Pdf];
        let Some(path) = dialogs::choose_save_path(
            self.window.upcast_ref(),
            "Save as PDF",
            &name,
            folder.as_deref(),
            &formats,
        )
        .await
        else {
            return false;
        };
        let path = dialogs::with_extension(path, &formats);

        let (document, revision, options) = {
            let mut state = self.state.borrow_mut();
            state.saving = true;
            let title = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let options = PdfOptions {
                jpeg_quality: state.config.jpeg_quality,
                title,
            };
            (state.document.clone(), state.document.revision(), options)
        };
        self.set_status(&format!("Saving page 1 of {page_count}…"));
        self.update_actions();

        let (sender, receiver) = async_channel::unbounded::<usize>();
        let target = path.clone();
        let saving = jobs::background(move || {
            let data = document.to_pdf(options, |done| {
                let _ = sender.send_blocking(done);
            })?;
            export::write_atomically(&target, &data)
        });
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(done) = receiver.recv().await {
                let Some(app) = app.upgrade() else { return };
                if done < page_count {
                    app.set_status(&format!("Saving page {} of {page_count}…", done + 1));
                }
            }
        });
        let result = saving.await;

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        {
            let mut state = self.state.borrow_mut();
            state.saving = false;
            if result.is_ok() {
                state.document.mark_saved(revision);
                state.config.last_directory = path.parent().map(PathBuf::from);
            }
        }
        self.update_actions();
        let close_now = self.close_after_save.take();
        match result {
            Ok(()) => {
                self.set_status(&format!("Saved {file_name}."));
                if close_now {
                    // Asks again if pages were added during the save.
                    self.window.close();
                }
                true
            }
            Err(err) => {
                self.set_status("Saving failed.");
                self.show_error(
                    &format!("Cannot save {file_name}"),
                    &dialogs::capitalize(&err.to_string()),
                );
                false
            }
        }
    }

    fn spawn_export_page(self: &Rc<Self>) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            let (page, number, folder) = {
                let state = app.state.borrow();
                let Some(index) = state.page_index() else {
                    return;
                };
                (
                    state.document.pages()[index].clone(),
                    index + 1,
                    state.config.last_directory.clone(),
                )
            };
            let formats = [FileFormat::Png, FileFormat::Jpeg, FileFormat::Pdf];
            let name = format!("page-{number}.png");
            let Some(path) = dialogs::choose_save_path(
                app.window.upcast_ref(),
                "Export Page",
                &name,
                folder.as_deref(),
                &formats,
            )
            .await
            else {
                return;
            };
            let path = dialogs::with_extension(path, &formats);
            let format = FileFormat::from_path(&path).unwrap_or(FileFormat::Png);
            let quality = app.state.borrow().config.jpeg_quality;
            app.set_status(&format!("Exporting page {number}…"));

            let target = path.clone();
            let result = jobs::background(move || -> Result<(), export::ExportError> {
                let data = match format {
                    FileFormat::Pdf => {
                        let mut pdf = export::PdfBuilder::new(PdfOptions {
                            jpeg_quality: quality,
                            title: None,
                        });
                        page.add_to_pdf(&mut pdf)?;
                        pdf.finish()?
                    }
                    FileFormat::Png => export::encode_png(&page.render()?)?,
                    FileFormat::Jpeg => export::encode_jpeg(&page.render()?, quality)?,
                };
                export::write_atomically(&target, &data)
            })
            .await;

            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match result {
                Ok(()) => {
                    app.state.borrow_mut().config.last_directory = path.parent().map(PathBuf::from);
                    app.set_status(&format!("Exported {file_name}."));
                }
                Err(err) => {
                    app.set_status("Export failed.");
                    app.show_error(
                        &format!("Cannot export {file_name}"),
                        &dialogs::capitalize(&err.to_string()),
                    );
                }
            }
        });
    }

    // ----------------------------------------------------------- rendering

    fn display(&self) -> Display {
        let mut state = self.state.borrow_mut();
        let selection = state.selection;
        match selection {
            Selection::Preview => {
                if state.source == InputSource::Feeder {
                    return Display::Message(
                        "The document feeder has no preview.\nPress Scan to scan all sheets in the feeder.",
                    );
                }
                match (&state.preview, &state.connection) {
                    (Some(preview), _) => Display::Ready {
                        selection,
                        source: preview.clone(),
                        adjustments: state.config.template,
                        full: None,
                        full_width: preview.raster.dimensions().0,
                    },
                    (None, Connection::Connected { .. }) => {
                        Display::Message("Place a document on the glass and press Preview.")
                    }
                    (None, Connection::Connecting(_)) => {
                        Display::Message("Connecting to the scanner…")
                    }
                    (None, _) => Display::Message("Choose a scanner to start."),
                }
            }
            Selection::Page(id) => {
                let Some(page) = state.document.get(id) else {
                    return Display::Message("");
                };
                let (adjustments, image) = (page.adjustments, page.image.clone());
                let cached = state
                    .full_image
                    .as_ref()
                    .filter(|(p, _)| *p == id)
                    .map(|(_, b)| b.clone());
                match state.proxy(id) {
                    Some(source) => Display::Ready {
                        selection,
                        source,
                        adjustments,
                        full_width: image.width,
                        full: Some((id, image, cached)),
                    },
                    None => Display::NeedsProxy(id, image),
                }
            }
        }
    }

    /// Re-renders the canvas. Requests made while a render runs are merged
    /// into one, and results for an entry that is no longer selected are
    /// dropped.
    fn request_render(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if state.render_running {
                state.render_pending = true;
                return;
            }
        }
        match self.display() {
            Display::Message(text) => {
                self.canvas.set_image(None);
                self.canvas.set_crop(None);
                self.canvas.set_placeholder(text);
            }
            Display::NeedsProxy(id, image) => {
                self.state.borrow_mut().render_running = true;
                let app = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    let proxy = jobs::background(move || {
                        let size = Target::Fit {
                            width: PROXY_SIZE,
                            height: PROXY_SIZE,
                        };
                        image
                            .decode()
                            .map(|full| imaging::render(&full, &Adjustments::default(), size))
                    })
                    .await;
                    let Some(app) = app.upgrade() else { return };
                    let (failed, stale) = {
                        let mut state = app.state.borrow_mut();
                        state.render_running = false;
                        let pending = std::mem::take(&mut state.render_pending);
                        let stale = pending || state.selection != Selection::Page(id);
                        match proxy {
                            Ok(proxy) => {
                                state.store_proxy(id, Arc::new(proxy));
                                (None, stale)
                            }
                            Err(err) => (Some(err), stale),
                        }
                    };
                    match failed {
                        // Render whatever is selected now.
                        None => app.request_render(),
                        Some(_) if stale => app.request_render(),
                        Some(err) => {
                            app.canvas.set_placeholder("This page cannot be displayed.");
                            app.set_status(&format!("Cannot display the page: {err}"));
                        }
                    }
                });
            }
            Display::Ready {
                selection,
                source,
                adjustments,
                full,
                full_width,
            } => {
                let Some(viewport) = self.canvas.viewport() else {
                    return;
                };
                self.state.borrow_mut().render_running = true;
                let app = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    let rendered = jobs::background(move || {
                        render_viewport(&source, adjustments, full, full_width, viewport)
                    })
                    .await;
                    let Some(app) = app.upgrade() else { return };
                    let (current, pending) = {
                        let mut state = app.state.borrow_mut();
                        state.render_running = false;
                        let current = state.selection == selection;
                        if current && let Ok(rendered) = &rendered {
                            state.auto_threshold = rendered.threshold;
                            if let Some(decoded) = &rendered.decoded_full {
                                state.full_image = Some(decoded.clone());
                            }
                        }
                        (current, std::mem::take(&mut state.render_pending))
                    };
                    match rendered {
                        Ok(rendered) if current => {
                            app.canvas
                                .set_image(Some((&rendered.bitmap, viewport.region)));
                            // The crop may have changed while this was rendering.
                            let crop = app.state.borrow().selected_adjustments().crop;
                            app.canvas.sync_crop(crop);
                            app.refresh_panel();
                        }
                        Ok(_) => {}
                        Err(err) => app.set_status(&format!("Cannot display the page: {err}")),
                    }
                    if pending || !current {
                        app.request_render();
                    }
                });
            }
        }
    }

    fn queue_thumbnail(self: &Rc<Self>, id: PageId) {
        {
            let mut state = self.state.borrow_mut();
            if !state.thumbnail_queue.contains(&id) {
                state.thumbnail_queue.push_back(id);
            }
        }
        self.process_thumbnails();
    }

    fn process_thumbnails(self: &Rc<Self>) {
        let (id, image, adjustments, proxy) = {
            let mut state = self.state.borrow_mut();
            if state.thumbnail_running {
                return;
            }
            let Some(id) = state.thumbnail_queue.pop_front() else {
                return;
            };
            let Some(page) = state.document.get(id) else {
                drop(state);
                return self.process_thumbnails();
            };
            let (image, adjustments) = (page.image.clone(), page.adjustments);
            let proxy = state
                .proxies
                .iter()
                .find(|(p, _)| *p == id)
                .map(|(_, b)| b.clone());
            state.thumbnail_running = true;
            (id, image, adjustments, proxy)
        };
        let app = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let rendered = jobs::background(move || {
                let source = match proxy {
                    Some(proxy) => proxy,
                    None => Arc::new(image.decode()?),
                };
                let size = Target::Fit {
                    width: THUMBNAIL_PIXELS,
                    height: THUMBNAIL_PIXELS,
                };
                Ok::<_, imaging::DecodeError>(imaging::render(&source, &adjustments, size))
            })
            .await;
            let Some(app) = app.upgrade() else { return };
            app.state.borrow_mut().thumbnail_running = false;
            match rendered {
                Ok(bitmap) => {
                    let texture = texture_from_bitmap(&bitmap);
                    let index = {
                        let mut state = app.state.borrow_mut();
                        state.thumbnails.insert(id, texture.clone());
                        state.document.index_of(id)
                    };
                    if let Some(index) = index {
                        app.pages.set_thumbnail(index, &texture);
                    }
                }
                Err(err) => log::warn!("cannot render thumbnail: {err}"),
            }
            app.process_thumbnails();
        });
    }

    // ------------------------------------------------------------- refresh

    fn refresh_all(self: &Rc<Self>) {
        self.update_toolbar();
        self.refresh_page_list();
        self.refresh_selection();
        self.update_status();
    }

    /// Updates everything that depends on the selected entry.
    fn refresh_selection(self: &Rc<Self>) {
        // The crop rectangle is set again once the new image is rendered.
        self.canvas.set_crop(None);
        self.canvas.set_placeholder("");
        self.canvas.set_image(None);
        let size = content_size(&self.state.borrow());
        match size {
            // Resetting the view also requests a render.
            Some((width, height, enlarge)) => self.canvas.reset_view(width, height, enlarge),
            None => self.request_render(),
        }
        self.refresh_panel();
        self.update_actions();
    }

    fn update_toolbar(&self) {
        let (names, selected, sources, source, connected, discovering) = {
            let state = self.state.borrow();
            let names: Vec<String> = state.scanners.iter().map(|s| s.name.clone()).collect();
            let selected = match state.connection {
                Connection::Connecting(i)
                | Connection::Connected { index: i, .. }
                | Connection::Failed(i) => Some(i),
                Connection::None => None,
            };
            let sources = state
                .caps()
                .map_or_else(|| vec![state.source], |caps| caps.sources());
            let connected = state.caps().is_some() && state.busy.is_none();
            (
                names,
                selected,
                sources,
                state.source,
                connected,
                state.discovering,
            )
        };
        self.updating_toolbar.set(true);
        let labels: Vec<&str> = if names.is_empty() {
            vec![if discovering {
                "Searching…"
            } else {
                "No scanner"
            }]
        } else {
            names.iter().map(String::as_str).collect()
        };
        set_dropdown_items(&self.scanner_dropdown, &labels);
        let selected = selected.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32);
        if self.scanner_dropdown.selected() != selected {
            self.scanner_dropdown.set_selected(selected);
        }
        self.scanner_dropdown.set_sensitive(!names.is_empty());
        self.refresh_button.set_sensitive(!discovering);

        let source_labels: Vec<&str> = sources.iter().map(|s| s.label()).collect();
        set_dropdown_items(&self.source_dropdown, &source_labels);
        let index = sources.iter().position(|s| *s == source).unwrap_or(0) as u32;
        if self.source_dropdown.selected() != index {
            self.source_dropdown.set_selected(index);
        }
        self.source_dropdown
            .set_sensitive(connected && sources.len() > 1);
        *self.source_options.borrow_mut() = sources;
        self.updating_toolbar.set(false);
        self.update_toolbar_sensitivity();
    }

    fn refresh_page_list(&self) {
        let (summaries, selection, thumbnails) = {
            let state = self.state.borrow();
            let summaries: Vec<PageSummary> = state
                .document
                .pages()
                .iter()
                .map(|page| PageSummary {
                    detail: format!("{} dpi · {}", page.image.dpi, page.adjustments.mode.label()),
                })
                .collect();
            let selection = match state.page_index() {
                Some(index) => ListSelection::Page(index),
                None => ListSelection::Preview,
            };
            let thumbnails: Vec<Option<gdk::Texture>> = state
                .document
                .pages()
                .iter()
                .map(|p| state.thumbnails.get(&p.id).cloned())
                .collect();
            (summaries, selection, thumbnails)
        };
        self.pages.set_pages(&summaries, selection);
        for (index, texture) in thumbnails.iter().enumerate() {
            if let Some(texture) = texture {
                self.pages.set_thumbnail(index, texture);
            }
        }
    }

    fn refresh_panel(&self) {
        let panel_state = {
            let state = self.state.borrow();
            let adjustments = state.selected_adjustments();
            let rotated = |(w, h): (f32, f32)| {
                if adjustments.rotation.is_sideways() {
                    (h, w)
                } else {
                    (w, h)
                }
            };
            let crop_size = |(w, h): (f32, f32)| {
                (!adjustments.crop.is_full())
                    .then(|| (w * adjustments.crop.width(), h * adjustments.crop.height()))
            };
            match state.selection {
                Selection::Preview => {
                    let source_caps = state.caps().and_then(|caps| caps.source(state.source));
                    let bed_mm = source_caps.map(|s| {
                        let to_mm = |units: u32| units as f32 * 25.4 / UNITS_PER_INCH as f32;
                        rotated((to_mm(s.max_width), to_mm(s.max_height)))
                    });
                    PanelState {
                        adjustments,
                        allowed_modes: OutputMode::ALL.to_vec(),
                        can_straighten: state.source == InputSource::Platen,
                        crop_size_mm: bed_mm.and_then(crop_size),
                        acquisition: source_caps.map(|s| Acquisition {
                            resolutions: s.resolutions.clone(),
                            resolution: s
                                .nearest_resolution(state.config.resolution)
                                .unwrap_or(state.config.resolution),
                            paper: (state.source == InputSource::Feeder)
                                .then_some(state.config.feeder_paper),
                        }),
                        auto_threshold: state.auto_threshold,
                    }
                }
                Selection::Page(id) => {
                    let page = state.document.get(id);
                    PanelState {
                        adjustments,
                        allowed_modes: page.map_or(OutputMode::ALL.to_vec(), |p| {
                            p.image.allowed_modes().to_vec()
                        }),
                        can_straighten: true,
                        crop_size_mm: page.and_then(|p| crop_size(rotated(p.image.size_mm()))),
                        acquisition: None,
                        auto_threshold: state.auto_threshold,
                    }
                }
            }
        };
        self.panel.show(panel_state);
    }

    fn update_actions(&self) {
        let enabled: Vec<(&str, bool)> = {
            let state = self.state.borrow();
            let connected = state.caps().is_some();
            let idle = state.busy.is_none();
            let page = state.page_index();
            let pages = state.document.len();
            let adjustments = state.selected_adjustments();
            let can_detect = match state.selection {
                Selection::Preview => {
                    state.preview.is_some() && state.source == InputSource::Platen
                }
                Selection::Page(_) => true,
            };
            let can_preview = state.caps().is_some_and(|c| c.platen.is_some())
                && state.source == InputSource::Platen;
            vec![
                ("preview", connected && idle && can_preview),
                ("scan", connected && idle),
                ("save", pages > 0 && !state.saving),
                ("export-page", page.is_some()),
                ("delete-page", page.is_some()),
                ("move-page-up", page.is_some_and(|i| i > 0)),
                ("move-page-down", page.is_some_and(|i| i + 1 < pages)),
                ("reset-crop", !adjustments.crop.is_full()),
                ("apply-to-all", pages > 0),
                ("auto-straighten", can_detect),
                ("detect-crop", can_detect),
                ("cancel", !idle),
                ("clear", pages > 0),
            ]
        };
        let actions = self.actions.borrow();
        for (name, value) in enabled {
            if let Some(action) = actions.get(name) {
                action.set_enabled(value);
            }
        }
        self.update_toolbar_sensitivity();
        self.update_status();
    }

    fn update_toolbar_sensitivity(&self) {
        let state = self.state.borrow();
        let idle = state.busy.is_none();
        self.scanner_dropdown
            .set_sensitive(idle && !state.scanners.is_empty());
        self.source_dropdown
            .set_sensitive(idle && state.caps().is_some_and(|c| c.sources().len() > 1));
    }

    fn set_status(&self, text: &str) {
        self.state.borrow_mut().status = text.to_owned();
        self.update_status();
    }

    fn update_status(&self) {
        let (text, working) = {
            let state = self.state.borrow();
            let working = state.busy.is_some()
                || state.saving
                || state.discovering
                || matches!(state.connection, Connection::Connecting(_));
            (state.status.clone(), working)
        };
        // A scan shows its own progress bar instead of the spinner.
        let scanning = self.state.borrow().busy.is_some();
        self.status_label.set_text(&text);
        self.spinner.set_spinning(working && !scanning);
        self.spinner.set_visible(working && !scanning);
    }

    fn show_error(&self, message: &str, detail: &str) {
        dialogs::show_error(self.window.upcast_ref(), message, detail);
    }
}

/// The scanner to connect to without asking: the one used last time, or the
/// only one found. While discovery is still running, wait until a plain
/// HTTP endpoint is known, since HTTPS announcements often arrive first.
fn auto_connect_candidate(state: &State, discovery_finished: bool) -> Option<usize> {
    if !matches!(state.connection, Connection::None | Connection::Failed(_)) {
        return None;
    }
    let remembered = state.config.scanner.as_ref().and_then(|saved| {
        state.scanners.iter().position(|s| {
            (saved.uuid.is_some() && s.uuid == saved.uuid)
                || s.endpoints.iter().any(|e| e.base_url() == saved.endpoint)
        })
    });
    let index = remembered.or((state.scanners.len() == 1).then_some(0))?;
    let has_http = state.scanners[index].endpoints.iter().any(|e| !e.tls);
    (has_http || discovery_finished).then_some(index)
}

/// An image to analyze, possibly still to be decoded.
enum AnalysisImage {
    Ready(Arc<Bitmap>),
    Original(Arc<ScannedImage>),
}

impl AnalysisImage {
    /// The image, decoded and downscaled if needed. Runs on a worker thread.
    fn load(self) -> Result<Arc<Bitmap>, imaging::DecodeError> {
        match self {
            AnalysisImage::Ready(bitmap) => Ok(bitmap),
            AnalysisImage::Original(image) => {
                let size = Target::Fit {
                    width: PROXY_SIZE,
                    height: PROXY_SIZE,
                };
                Ok(Arc::new(imaging::render(
                    &image.decode()?,
                    &Adjustments::default(),
                    size,
                )))
            }
        }
    }
}

/// The straightening (degrees) that levels `source` once rotated by `rotation`.
fn detect_tilt_of(source: &Bitmap, rotation: Rotation) -> Option<f32> {
    let gray = Adjustments {
        mode: OutputMode::Gray,
        rotation,
        ..Adjustments::default()
    };
    match imaging::render(source, &gray, Target::Full).raster {
        imaging::Raster::Gray(gray) => imaging::detect_skew(&gray),
        _ => None,
    }
}

/// The result of rendering the visible part of an image.
struct RenderedView {
    bitmap: Bitmap,
    threshold: Option<u8>,
    /// A full-resolution decode made for this render, to cache.
    decoded_full: Option<(PageId, Arc<Bitmap>)>,
}

/// Renders what the canvas shows: the whole image from `source` when it is
/// detailed enough, otherwise only the visible region, from the original.
fn render_viewport(
    source: &Bitmap,
    adjustments: Adjustments,
    full: Option<(PageId, Arc<ScannedImage>, Option<Arc<Bitmap>>)>,
    full_width: u32,
    viewport: Viewport,
) -> Result<RenderedView, imaging::DecodeError> {
    let target = Target::Fit {
        width: viewport.width,
        height: viewport.height,
    };
    // Pixels of `source` per pixel of the original image.
    let source_scale = f64::from(source.raster.dimensions().0) / f64::from(full_width.max(1));
    let needs_original = viewport.scale > source_scale * 1.05 && full.is_some();

    if viewport.region.is_full() && !needs_original {
        let view = imaging::render_view(source, &adjustments, target);
        return Ok(RenderedView {
            bitmap: view.bitmap,
            threshold: view.threshold,
            decoded_full: None,
        });
    }

    // The automatic threshold must come from the whole crop area, not just
    // the visible part; the downscaled copy is enough to compute it.
    let threshold = match (adjustments.mode, adjustments.threshold) {
        (OutputMode::BlackAndWhite, imaging::Threshold::Auto) => {
            let small = Target::Fit {
                width: 800,
                height: 800,
            };
            imaging::render_view(source, &adjustments, small).threshold
        }
        (OutputMode::BlackAndWhite, imaging::Threshold::Manual(level)) => Some(level),
        _ => None,
    };
    let mut decoded_full = None;
    let original = match full {
        Some((_, _, Some(cached))) if needs_original => Some(cached),
        Some((id, image, None)) if needs_original => {
            let decoded = Arc::new(image.decode()?);
            decoded_full = Some((id, decoded.clone()));
            Some(decoded)
        }
        _ => None,
    };
    let region = Adjustments {
        crop: viewport.region,
        threshold: threshold.map_or(adjustments.threshold, imaging::Threshold::Manual),
        ..adjustments
    };
    let bitmap = imaging::render(original.as_deref().unwrap_or(source), &region, target);
    Ok(RenderedView {
        bitmap,
        threshold,
        decoded_full,
    })
}

/// Full-resolution size of the selected image as shown (rotated), and
/// whether fitting it to the window may enlarge it.
fn content_size(state: &State) -> Option<(u32, u32, bool)> {
    let (width, height, rotation, enlarge) = match state.selection {
        Selection::Preview => {
            let (w, h) = state.preview.as_ref()?.raster.dimensions();
            // The preview is low resolution by design: always fill the space.
            (w, h, state.config.template.rotation, true)
        }
        Selection::Page(id) => {
            let page = state.document.get(id)?;
            (
                page.image.width,
                page.image.height,
                page.adjustments.rotation,
                false,
            )
        }
    };
    Some(if rotation.is_sideways() {
        (height, width, enlarge)
    } else {
        (width, height, enlarge)
    })
}

/// Replaces a dropdown's items, but only when they differ: replacing the
/// model closes an open popup, and doing it while the dropdown handles a
/// selection crashes GTK.
fn set_dropdown_items(dropdown: &gtk::DropDown, labels: &[&str]) {
    let unchanged = dropdown
        .model()
        .and_downcast::<gtk::StringList>()
        .is_some_and(|model| {
            model.n_items() as usize == labels.len()
                && labels
                    .iter()
                    .enumerate()
                    .all(|(i, label)| model.string(i as u32).as_deref() == Some(*label))
        });
    if !unchanged {
        dropdown.set_model(Some(&gtk::StringList::new(labels)));
    }
}

/// Creates a GPU-uploadable texture from a bitmap.
fn texture_from_bitmap(bitmap: &Bitmap) -> gdk::Texture {
    let rgb = bitmap.raster.to_rgb();
    let (width, height) = rgb.dimensions();
    let bytes = glib::Bytes::from_owned(rgb.into_raw());
    gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8,
        &bytes,
        width as usize * 3,
    )
    .upcast()
}

/// Today's date as `YYYY-MM-DD`, for default file names.
fn today() -> String {
    glib::DateTime::now_local()
        .and_then(|now| now.format("%Y-%m-%d"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "document".to_owned())
}
