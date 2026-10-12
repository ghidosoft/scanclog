//! The sidebar listing the preview entry and the scanned pages.

use gtk::prelude::*;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const THUMBNAIL_SIZE: i32 = 140;

/// Which entry of the list is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSelection {
    /// The preview and template for new scans.
    Preview,
    /// A page, by position.
    Page(usize),
}

/// What a page row shows besides its thumbnail.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSummary {
    pub detail: String,
}

struct Callbacks {
    selected: Option<Box<dyn Fn(ListSelection)>>,
    moved: Option<Box<dyn Fn(usize, usize)>>,
}

#[derive(Clone)]
pub struct PageList {
    root: gtk::Box,
    list: gtk::ListBox,
    pictures: Rc<RefCell<Vec<gtk::Picture>>>,
    /// Set while rows are rebuilt, so programmatic changes emit no signals.
    updating: Rc<Cell<bool>>,
    callbacks: Rc<RefCell<Callbacks>>,
}

impl PageList {
    pub fn new() -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .build();
        list.add_css_class("navigation-sidebar");
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let buttons = gtk::Box::builder()
            .spacing(4)
            .margin_top(6)
            .margin_bottom(6)
            .halign(gtk::Align::Center)
            .build();
        for (icon, action, tooltip) in [
            ("go-up-symbolic", "win.move-page-up", "Move page up"),
            ("go-down-symbolic", "win.move-page-down", "Move page down"),
            ("user-trash-symbolic", "win.delete-page", "Delete page"),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .action_name(action)
                .tooltip_text(tooltip)
                .build();
            button.add_css_class("flat");
            buttons.append(&button);
        }

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(THUMBNAIL_SIZE + 40);
        root.append(&scrolled);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&buttons);

        let page_list = PageList {
            root,
            list,
            pictures: Rc::default(),
            updating: Rc::default(),
            callbacks: Rc::new(RefCell::new(Callbacks {
                selected: None,
                moved: None,
            })),
        };

        let this = page_list.clone();
        page_list.list.connect_row_selected(move |_, row| {
            if this.updating.get() {
                return;
            }
            let Some(row) = row else { return };
            let selection = row_selection(row);
            if let Some(callback) = &this.callbacks.borrow().selected {
                callback(selection);
            }
        });

        let drop_target = gtk::DropTarget::new(glib::Type::U32, gdk::DragAction::MOVE);
        let this = page_list.clone();
        drop_target.connect_drop(move |_, value, _, y| {
            let Ok(from) = value.get::<u32>() else {
                return false;
            };
            let page_count = this.pictures.borrow().len();
            let to = match this.list.row_at_y(y as i32) {
                Some(row) => match row_selection(&row) {
                    ListSelection::Preview => 0,
                    ListSelection::Page(index) => index,
                },
                None => page_count.saturating_sub(1),
            };
            if let Some(callback) = &this.callbacks.borrow().moved {
                callback(from as usize, to);
            }
            true
        });
        page_list.list.add_controller(drop_target);

        page_list
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_selected(&self, callback: impl Fn(ListSelection) + 'static) {
        self.callbacks.borrow_mut().selected = Some(Box::new(callback));
    }

    /// Called with `(from, to)` page positions when a page is dragged.
    pub fn connect_moved(&self, callback: impl Fn(usize, usize) + 'static) {
        self.callbacks.borrow_mut().moved = Some(Box::new(callback));
    }

    /// Rebuilds the rows. Thumbnails are kept by position where possible and
    /// can be replaced with [`set_thumbnail`](Self::set_thumbnail).
    pub fn set_pages(&self, pages: &[PageSummary], selection: ListSelection) {
        self.updating.set(true);
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        self.list.append(&preview_row());

        let mut pictures = Vec::with_capacity(pages.len());
        for (index, summary) in pages.iter().enumerate() {
            let (row, picture) = page_row(index, summary);
            self.list.append(&row);
            pictures.push(picture);
        }
        *self.pictures.borrow_mut() = pictures;
        self.select(selection);
        self.updating.set(false);
    }

    pub fn set_thumbnail(&self, index: usize, texture: &gdk::Texture) {
        if let Some(picture) = self.pictures.borrow().get(index) {
            picture.set_paintable(Some(texture));
        }
    }

    /// Selects an entry without emitting the selection callback.
    pub fn select(&self, selection: ListSelection) {
        let was_updating = self.updating.replace(true);
        let index = match selection {
            ListSelection::Preview => 0,
            ListSelection::Page(page) => page as i32 + 1,
        };
        if let Some(row) = self.list.row_at_index(index) {
            // No grab_focus: rows are rebuilt while the user may be typing
            // in another control.
            self.list.select_row(Some(&row));
        }
        self.updating.set(was_updating);
    }
}

fn row_selection(row: &gtk::ListBoxRow) -> ListSelection {
    match row.index() {
        index if index <= 0 => ListSelection::Preview,
        index => ListSelection::Page(index as usize - 1),
    }
}

fn preview_row() -> gtk::ListBoxRow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let icon = gtk::Image::builder()
        .icon_name("scanner-symbolic")
        .pixel_size(48)
        .build();
    let title = gtk::Label::new(Some("Preview"));
    title.add_css_class("heading");
    let subtitle = gtk::Label::new(Some("Settings for new scans"));
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");
    content.append(&icon);
    content.append(&title);
    content.append(&subtitle);
    gtk::ListBoxRow::builder().child(&content).build()
}

fn page_row(index: usize, summary: &PageSummary) -> (gtk::ListBoxRow, gtk::Picture) {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .width_request(THUMBNAIL_SIZE)
        .height_request(THUMBNAIL_SIZE)
        .can_shrink(true)
        .build();
    let title = gtk::Label::new(Some(&format!("Page {}", index + 1)));
    title.add_css_class("heading");
    let detail = gtk::Label::new(Some(&summary.detail));
    detail.add_css_class("dim-label");
    detail.add_css_class("caption");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    content.append(&picture);
    content.append(&title);
    content.append(&detail);
    let row = gtk::ListBoxRow::builder().child(&content).build();

    let drag = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .build();
    let position = index as u32;
    drag.connect_prepare(move |_, _, _| {
        Some(gdk::ContentProvider::for_value(&position.to_value()))
    });
    let source_row = row.clone();
    drag.connect_drag_begin(move |source, _| {
        let icon = gtk::WidgetPaintable::new(Some(&source_row));
        source.set_icon(Some(&icon), 0, 0);
    });
    row.add_controller(drag);

    (row, picture)
}
