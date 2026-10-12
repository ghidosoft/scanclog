//! The GTK user interface.

mod canvas;
mod dialogs;
mod jobs;
mod page_list;
mod panel;
mod window;

use gtk::prelude::*;
use gtk::{gdk, glib};

pub const APP_ID: &str = "io.github.ghidosoft.ScanClog";

const CSS: &str = "
.scanclog-canvas {
    background-color: alpha(currentColor, 0.06);
}
";

pub fn run() -> glib::ExitCode {
    let application = gtk::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    application.connect_activate(|application| {
        // A second launch just raises the existing window.
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        window::App::new(application).present();
    });
    // Command-line arguments were already handled by clap.
    application.run_with_args::<&str>(&[])
}
