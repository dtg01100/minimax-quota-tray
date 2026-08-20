//! Minimal direct FFI to libayatana-appindicator3. We dlopen the .so at
//! runtime and bind just the 5 symbols we need. This avoids the gtk-0.15
//! dependency that the published libayatana-appindicator crate pulls in
//! (which conflicts with our gtk 0.9).

use anyhow::{Context, Result};
use gtk::prelude::*;
use libloading::Library;
use std::sync::OnceLock;

// ToGlibPtr is in the glib crate directly (gtk-rs re-exports its own
// glib under `gtk::glib`, but those are private — use the top-level).
// But gtk-rs depends on a different glib version (0.10 vs our 0.20).
// We can't use ToGlibPtr across versions, so instead do the conversion
// manually via glib's GObject cast.

const LIB_NAMES: &[&str] = &[
    "libayatana-appindicator3.so.1",
    "libayatana-appindicator3.so",
];

const CATEGORY_APPLICATION_STATUS: u32 = 0;
const STATUS_ACTIVE: u32 = 1;

/// Function pointers we use. Resolved at runtime via dlopen.
struct IndicatorLib {
    _lib: Library,
    new: unsafe extern "C" fn(id: *const i8, icon: *const i8, category: u32) -> *mut (),
    set_status: unsafe extern "C" fn(ind: *mut (), status: u32),
    set_menu: unsafe extern "C" fn(ind: *mut (), menu: *mut ()),
    set_icon_full: unsafe extern "C" fn(ind: *mut (), name: *const i8, desc: *const i8),
    set_title: unsafe extern "C" fn(ind: *mut (), title: *const i8),
}

static LIB: OnceLock<Result<IndicatorLib, String>> = OnceLock::new();

fn load() -> Result<&'static IndicatorLib> {
    let r = LIB.get_or_init(|| -> Result<IndicatorLib, String> {
        let mut last_err = String::new();
        for name in LIB_NAMES {
            match unsafe { Library::new(name) } {
                Ok(lib) => match resolve_symbols(lib) {
                    Ok(ind) => return Ok(ind),
                    Err(e) => last_err = format!("{name}: {e:?}"),
                },
                Err(e) => last_err = format!("{name}: {e}"),
            }
        }
        Err(last_err)
    });
    r.as_ref().map_err(|e| anyhow::anyhow!("loading libayatana-appindicator: {e}"))
}

fn resolve_symbols(lib: Library) -> Result<IndicatorLib> {
    unsafe {
        let new = *lib.get::<unsafe extern "C" fn(*const i8, *const i8, u32) -> *mut ()>(b"app_indicator_new\0")
            .context("app_indicator_new")?;
        let set_status = *lib.get::<unsafe extern "C" fn(*mut (), u32)>(b"app_indicator_set_status\0")
            .context("app_indicator_set_status")?;
        let set_menu = *lib.get::<unsafe extern "C" fn(*mut (), *mut ())>(b"app_indicator_set_menu\0")
            .context("app_indicator_set_menu")?;
        let set_icon_full = *lib.get::<unsafe extern "C" fn(*mut (), *const i8, *const i8)>(b"app_indicator_set_icon_full\0")
            .context("app_indicator_set_icon_full")?;
        let set_title = *lib.get::<unsafe extern "C" fn(*mut (), *const i8)>(b"app_indicator_set_title\0")
            .context("app_indicator_set_title")?;
        Ok(IndicatorLib { _lib: lib, new, set_status, set_menu, set_icon_full, set_title })
    }
}

/// Wrapper around a `AppIndicator*`. Methods take `&mut self` because the
/// underlying C calls mutate state.
pub struct AppIndicator {
    ptr: *mut (),
}

impl AppIndicator {
    pub fn new(id: &str, icon: &str) -> Result<Self> {
        let lib = load()?;
        let id_c = std::ffi::CString::new(id).context("invalid id")?;
        let icon_c = std::ffi::CString::new(icon).context("invalid icon")?;
        let ptr = unsafe { (lib.new)(id_c.as_ptr(), icon_c.as_ptr(), CATEGORY_APPLICATION_STATUS) };
        Ok(Self { ptr })
    }

    pub fn set_status_active(&mut self) -> Result<()> {
        let lib = load()?;
        unsafe { (lib.set_status)(self.ptr, STATUS_ACTIVE) };
        Ok(())
    }

    pub fn set_menu<M: IsA<gtk::Menu>>(&mut self, menu: &M) -> Result<()> {
        let lib = load()?;
        // Any gtk::Menu is a GObject at the C level; the menu pointer is
        // cast to *mut () for the C call. Avoids ToGlibPtr which differs
        // between gtk-rs's internal glib (0.10) and our direct glib (0.20).
        let menu_ptr = menu.as_ptr() as *mut ();
        unsafe { (lib.set_menu)(self.ptr, menu_ptr) };
        Ok(())
    }

    pub fn set_icon_full(&mut self, name: &str, desc: &str) -> Result<()> {
        let lib = load()?;
        let name_c = std::ffi::CString::new(name).context("invalid icon name")?;
        let desc_c = std::ffi::CString::new(desc).context("invalid desc")?;
        unsafe { (lib.set_icon_full)(self.ptr, name_c.as_ptr(), desc_c.as_ptr()) };
        Ok(())
    }

    pub fn set_title(&mut self, title: &str) -> Result<()> {
        let lib = load()?;
        let title_c = std::ffi::CString::new(title).context("invalid title")?;
        unsafe { (lib.set_title)(self.ptr, title_c.as_ptr()) };
        Ok(())
    }
}

// Send + !Sync — the indicator owns its own D-Bus connection; not safe
// to share across threads. Always manipulate from the GLib main thread.
unsafe impl Send for AppIndicator {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn load_indicator_library() {
        assert!(load().is_ok());
    }

    #[test]
    #[ignore]
    fn create_indicator() {
        let ind = AppIndicator::new("test", "dialog-information-symbolic");
        assert!(ind.is_ok());
    }
}