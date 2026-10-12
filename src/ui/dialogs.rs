//! Modal dialogs.

use gtk::prelude::*;
use gtk::{gio, glib};
use scanclog::escl::Endpoint;
use scanclog::export::FileFormat;
use std::path::{Path, PathBuf};

/// Asks where to save a file. Returns `None` if the user cancels.
pub async fn choose_save_path(
    window: &gtk::Window,
    title: &str,
    initial_name: &str,
    folder: Option<&Path>,
    formats: &[FileFormat],
) -> Option<PathBuf> {
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    for format in formats {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(format.label()));
        filter.add_mime_type(format.mime_type());
        filters.append(&filter);
    }
    let dialog = gtk::FileDialog::builder()
        .title(title)
        .modal(true)
        .initial_name(initial_name)
        .filters(&filters)
        .build();
    // Default to the user's Documents folder rather than the working
    // directory, which is meaningless for a desktop application.
    let documents = glib::user_special_dir(glib::UserDirectory::Documents);
    let folder = folder
        .map(Path::to_path_buf)
        .filter(|f| f.is_dir())
        .or(documents);
    if let Some(folder) = folder {
        dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
    }
    let file = dialog.save_future(Some(window)).await.ok()?;
    file.path()
}

/// Makes sure `path` ends in an extension of one of `formats`, appending the
/// first one otherwise.
pub fn with_extension(path: PathBuf, formats: &[FileFormat]) -> PathBuf {
    match FileFormat::from_path(&path) {
        Some(format) if formats.contains(&format) => path,
        _ => {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".");
            name.push(formats[0].extension());
            path.with_file_name(name)
        }
    }
}

/// What to do with unsaved pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsavedChoice {
    Cancel,
    Discard,
    Save,
}

pub async fn confirm_unsaved(window: &gtk::Window, pages: usize, action: &str) -> UnsavedChoice {
    let message = if pages == 1 {
        "Save the scanned page?".to_owned()
    } else {
        format!("Save the {pages} scanned pages?")
    };
    let alert = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail("Unsaved pages are lost when you continue.")
        .buttons(["Cancel", action, "Save as PDF…"])
        .cancel_button(0)
        .default_button(2)
        .build();
    match alert.choose_future(Some(window)).await {
        Ok(1) => UnsavedChoice::Discard,
        Ok(2) => UnsavedChoice::Save,
        _ => UnsavedChoice::Cancel,
    }
}

pub fn show_error(window: &gtk::Window, message: &str, detail: &str) {
    let alert = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail(detail)
        .build();
    alert.show(Some(window));
}

/// Asks for a scanner address. Returns `None` if the user cancels.
pub async fn ask_scanner_address(window: &gtk::Window) -> Option<Endpoint> {
    let (sender, receiver) = async_channel::bounded(1);

    let entry = gtk::Entry::builder()
        .placeholder_text("192.168.1.20 or http://scanner.local/eSCL")
        .activates_default(true)
        .hexpand(true)
        .build();
    let error = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .build();
    error.add_css_class("error");
    let explanation = gtk::Label::builder()
        .label("Enter the address of a scanner that supports eSCL (AirScan), for when it is not found automatically.")
        .wrap(true)
        .xalign(0.0)
        .max_width_chars(48)
        .build();
    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let add = gtk::Button::with_mnemonic("_Add");
    add.add_css_class("suggested-action");
    let buttons = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(6)
        .build();
    buttons.append(&cancel);
    buttons.append(&add);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&explanation);
    content.append(&entry);
    content.append(&error);
    content.append(&buttons);

    let dialog = gtk::Window::builder()
        .title("Add Scanner")
        .modal(true)
        .transient_for(window)
        .resizable(false)
        .default_widget(&add)
        .child(&content)
        .build();

    let close = {
        let sender = sender.clone();
        move |dialog: &gtk::Window, result: Option<Endpoint>| {
            let _ = sender.try_send(result);
            dialog.destroy();
        }
    };
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[strong]
        close,
        move |_| close(&dialog, None)
    ));
    add.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        entry,
        #[weak]
        error,
        #[strong]
        close,
        move |_| match Endpoint::parse(&entry.text()) {
            Ok(endpoint) => close(&dialog, Some(endpoint)),
            Err(err) => {
                error.set_text(&capitalize(&err.to_string()));
                error.set_visible(true);
            }
        }
    ));
    dialog.connect_close_request(move |_| {
        let _ = sender.try_send(None);
        glib::Propagation::Proceed
    });
    dialog.present();
    receiver.recv().await.ok().flatten()
}

pub fn show_about(window: &gtk::Window) {
    gtk::AboutDialog::builder()
        .transient_for(window)
        .modal(true)
        .program_name("ScanClog")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Scan documents from network scanners, with non-destructive adjustments.")
        .logo_icon_name("scanner")
        .build()
        .present();
}

/// Error messages from the library start in lower case; dialogs show
/// sentences.
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_missing_extensions() {
        let pdf = [FileFormat::Pdf];
        assert_eq!(
            with_extension("a/scan".into(), &pdf),
            PathBuf::from("a/scan.pdf")
        );
        assert_eq!(
            with_extension("scan.PDF".into(), &pdf),
            PathBuf::from("scan.PDF")
        );
        assert_eq!(
            with_extension("scan.png".into(), &pdf),
            PathBuf::from("scan.png.pdf")
        );
        let images = [FileFormat::Png, FileFormat::Jpeg];
        assert_eq!(
            with_extension("page.jpg".into(), &images),
            PathBuf::from("page.jpg")
        );
    }

    #[test]
    fn capitalizes_messages() {
        assert_eq!(capitalize("the scanner is busy"), "The scanner is busy");
        assert_eq!(capitalize(""), "");
    }
}
