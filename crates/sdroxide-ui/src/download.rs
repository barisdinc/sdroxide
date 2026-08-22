//! Save a file from the UI. Native pops a "Save As" dialog; wasm triggers a
//! browser download via a Blob + anchor click.

/// The MIME type a browser download is labelled with. Native ignores it — the
/// name and the bytes are all a filesystem needs — but a browser hands the file
/// to whatever the type says, so a picture saved as `text/plain` opens in a text
/// editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mime {
    Text,
    Png,
}

impl Mime {
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // only the browser labels a download
    fn as_str(self) -> &'static str {
        match self {
            Mime::Text => "text/plain",
            Mime::Png => "image/png",
        }
    }
}

/// Save `data` under a suggested `name`, as a text file.
pub fn save(name: &str, data: &[u8]) {
    save_as(name, data, Mime::Text);
}

/// Save `data` under a suggested `name`, labelled as `mime`.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_as(name: &str, data: &[u8], _mime: Mime) {
    let data = data.to_vec();
    let name = name.to_string();
    // rfd's dialog is blocking; run it off the UI thread.
    std::thread::Builder::new()
        .name("sdroxide-save".into())
        .spawn(move || {
            if let Some(path) = rfd::FileDialog::new().set_file_name(&name).save_file() {
                if let Err(e) = std::fs::write(&path, &data) {
                    eprintln!("sdroxide: saving {}: {e}", path.display());
                }
            }
        })
        .ok();
}

/// Open a text file via a native "Open" dialog (off the UI thread) and store
/// its contents into `inbox` for the UI to pick up next frame. Native only —
/// the browser client has no filesystem picker here.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_text(
    filter_name: &str,
    ext: &str,
    inbox: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) {
    let filter_name = filter_name.to_string();
    let ext = ext.to_string();
    std::thread::Builder::new()
        .name("sdroxide-open".into())
        .spawn(move || {
            if let Some(path) =
                rfd::FileDialog::new().add_filter(&filter_name, &[ext.as_str()]).pick_file()
            {
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        if let Ok(mut g) = inbox.lock() {
                            *g = Some(text);
                        }
                    }
                    Err(e) => eprintln!("sdroxide: reading {}: {e}", path.display()),
                }
            }
        })
        .ok();
}

#[cfg(target_arch = "wasm32")]
pub fn save_as(name: &str, data: &[u8], mime: Mime) {
    use wasm_bindgen::JsCast;

    let array = js_sys::Uint8Array::from(data);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime.as_str());
    let blob = match web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts) {
        Ok(b) => b,
        Err(_) => return,
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else { return };

    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    if let Ok(a) = doc.create_element("a") {
        let a: web_sys::HtmlAnchorElement = a.unchecked_into();
        a.set_href(&url);
        a.set_download(name);
        a.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}
