//! Raw libmpv bindings, loaded at runtime.
//!
//! `libmpv` is deliberately **not** a link-time dependency: the app builds on
//! machines without mpv, and falls back to an external player at runtime. We
//! therefore declare the small slice of the C API we need by hand and resolve
//! it with `libloading`.
//!
//! Every type and constant here was taken from mpv 0.41.0's `client.h` and
//! `render.h`. The `MPV_RENDER_PARAM_*` values are *not* stable across mpv
//! releases, so they come from `build.rs` (see `mpv_layout.rs`) and embedded
//! playback is refused unless the runtime library reports an API version we
//! have verified.

#![allow(non_camel_case_types, non_snake_case)]
// A C binding surface legitimately declares more than a given build uses; the
// version query and property getters exist for callers and for diagnostics.
#![allow(dead_code)]

use std::ffi::{c_char, c_double, c_int, c_ulong, c_void, CStr, CString};

use libloading::Library;

/// The values `build.rs` resolved (from the local headers, or the mpv 0.41
/// defaults).
mod layout {
    include!(concat!(env!("OUT_DIR"), "/mpv_layout.rs"));
}

pub use layout::{BLOCK_FOR_TARGET_TIME, SW_FORMAT, SW_POINTER, SW_SIZE, SW_STRIDE};

pub type MpvHandle = *mut c_void;
pub type MpvRenderContext = *mut c_void;

// ---------------------------------------------------------------- constants

pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;

pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_STOP: c_int = 2;
pub const MPV_END_FILE_REASON_QUIT: c_int = 3;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

/// `MPV_RENDER_API_TYPE_SW` from render.h.
pub const RENDER_API_TYPE_SW: &CStr = c"sw";

/// The client API version whose render-parameter layout we have verified.
/// API 2.5 == mpv 0.40.0 and later.
pub const VERIFIED_API_VERSION: c_ulong = (2 << 16) | 5;

// ------------------------------------------------------------------ structs

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_property {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_end_file {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct mpv_render_param {
    pub type_: c_int,
    pub data: *mut c_void,
}

impl mpv_render_param {
    pub fn new(type_: c_int, data: *mut c_void) -> Self {
        Self { type_, data }
    }
}

// ------------------------------------------------------------ function types

macro_rules! mpv_fns {
    ($( fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) $( -> $ret:ty )? ; )*) => {
        pub struct MpvLib {
            _lib: Library,
            $( pub $name: unsafe extern "C" fn($($ty),*) $( -> $ret )?, )*
        }

        impl MpvLib {
            /// Try to load libmpv from the usual names on each platform.
            pub fn load() -> Result<Self, crate::PlayerError> {
                let mut last_error = None;
                for name in CANDIDATE_LIBRARY_NAMES {
                    match unsafe { Library::new(name) } {
                        Ok(lib) => return Self::from_library(lib),
                        Err(e) => last_error = Some(e),
                    }
                }
                Err(crate::PlayerError::EmbeddedUnavailable(format!(
                    "libmpv could not be loaded ({}). Install mpv to enable embedded playback.",
                    last_error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "no candidate library found".into())
                )))
            }

            fn from_library(lib: Library) -> Result<Self, crate::PlayerError> {
                unsafe {
                    $(
                        let $name: unsafe extern "C" fn($($ty),*) $( -> $ret )? = {
                            let sym = lib.get(stringify!($name).as_bytes()).map_err(|e| {
                                crate::PlayerError::EmbeddedUnavailable(format!(
                                    "libmpv is missing symbol {}: {e}",
                                    stringify!($name)
                                ))
                            })?;
                            *sym
                        };
                    )*

                    let me = Self { _lib: lib, $($name,)* };

                    // The render-parameter enum moved between mpv releases, so
                    // only trust the pointer arithmetic on a version we know.
                    let api = (me.mpv_client_api_version)();
                    if api < VERIFIED_API_VERSION {
                        return Err(crate::PlayerError::EmbeddedUnavailable(format!(
                            "libmpv reports client API {}.{}, but reel needs >= 2.5 \
                             (mpv 0.40+) because the software renderer's parameter \
                             layout changed between releases",
                            api >> 16,
                            api & 0xffff
                        )));
                    }

                    Ok(me)
                }
            }
        }
    };
}

/// Library file names to try, in order. `libmpv.so.2` is the ABI-stable soname.
const CANDIDATE_LIBRARY_NAMES: &[&str] = &[
    "libmpv.so.2",
    "libmpv.so",
    "libmpv.2.dylib",
    "libmpv.dylib",
    "mpv-2.dll",
    "libmpv-2.dll",
];

mpv_fns! {
    fn mpv_create() -> MpvHandle;
    fn mpv_initialize(ctx: MpvHandle) -> c_int;
    fn mpv_terminate_destroy(ctx: MpvHandle);
    fn mpv_client_api_version() -> c_ulong;
    fn mpv_error_string(error: c_int) -> *const c_char;
    fn mpv_set_option_string(ctx: MpvHandle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_set_property(ctx: MpvHandle, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    fn mpv_get_property(ctx: MpvHandle, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    fn mpv_get_property_string(ctx: MpvHandle, name: *const c_char) -> *mut c_char;
    fn mpv_free(data: *mut c_void);
    fn mpv_command(ctx: MpvHandle, args: *const *const c_char) -> c_int;
    fn mpv_observe_property(ctx: MpvHandle, reply_userdata: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_wait_event(ctx: MpvHandle, timeout: c_double) -> *mut mpv_event;
    fn mpv_set_wakeup_callback(ctx: MpvHandle, callback: unsafe extern "C" fn(*mut c_void), data: *mut c_void) -> c_int;
    fn mpv_render_context_create(res: *mut MpvRenderContext, ctx: MpvHandle, params: *mut mpv_render_param) -> c_int;
    fn mpv_render_context_render(ctx: MpvRenderContext, params: *mut mpv_render_param) -> c_int;
    fn mpv_render_context_update(ctx: MpvRenderContext) -> u64;
    fn mpv_render_context_set_update_callback(ctx: MpvRenderContext, callback: unsafe extern "C" fn(*mut c_void), data: *mut c_void);
    fn mpv_render_context_free(ctx: MpvRenderContext);
}

impl MpvLib {
    pub fn error_string(&self, code: c_int) -> String {
        unsafe {
            let ptr = (self.mpv_error_string)(code);
            if ptr.is_null() {
                return format!("mpv error {code}");
            }
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }

    /// Set an option. Must be called before `mpv_initialize`.
    pub fn set_option(&self, ctx: MpvHandle, name: &str, value: &str) -> Result<(), crate::PlayerError> {
        let (name, value) = (to_cstring(name)?, to_cstring(value)?);
        let rc = unsafe { (self.mpv_set_option_string)(ctx, name.as_ptr(), value.as_ptr()) };
        self.check(rc, || format!("setting mpv option {name:?}"))
    }

    pub fn set_double(&self, ctx: MpvHandle, name: &str, value: f64) -> Result<(), crate::PlayerError> {
        let name = to_cstring(name)?;
        let mut value = value;
        let rc = unsafe {
            (self.mpv_set_property)(
                ctx,
                name.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut value as *mut f64 as *mut c_void,
            )
        };
        self.check(rc, || format!("setting property {name:?}"))
    }

    pub fn set_flag(&self, ctx: MpvHandle, name: &str, value: bool) -> Result<(), crate::PlayerError> {
        let name = to_cstring(name)?;
        let mut value: c_int = value as c_int;
        let rc = unsafe {
            (self.mpv_set_property)(
                ctx,
                name.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut value as *mut c_int as *mut c_void,
            )
        };
        self.check(rc, || format!("setting flag {name:?}"))
    }

    /// Run a command given as argv, e.g. `["seek", "10", "absolute"]`.
    pub fn command(&self, ctx: MpvHandle, args: &[&str]) -> Result<(), crate::PlayerError> {
        let owned: Vec<CString> = args
            .iter()
            .map(|a| to_cstring(a))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());

        let rc = unsafe { (self.mpv_command)(ctx, ptrs.as_ptr()) };
        self.check(rc, || format!("running command {args:?}"))
    }

    pub fn observe(&self, ctx: MpvHandle, id: u64, name: &str, format: c_int) {
        if let Ok(name) = to_cstring(name) {
            unsafe { (self.mpv_observe_property)(ctx, id, name.as_ptr(), format) };
        }
    }

    /// Read a double property directly (used for one-off queries).
    pub fn get_double(&self, ctx: MpvHandle, name: &str) -> Option<f64> {
        let name = to_cstring(name).ok()?;
        let mut value: f64 = 0.0;
        let rc = unsafe {
            (self.mpv_get_property)(
                ctx,
                name.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut value as *mut f64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(value)
    }

    pub fn get_string(&self, ctx: MpvHandle, name: &str) -> Option<String> {
        let name = to_cstring(name).ok()?;
        unsafe {
            let ptr = (self.mpv_get_property_string)(ctx, name.as_ptr());
            if ptr.is_null() {
                return None;
            }
            let value = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            (self.mpv_free)(ptr as *mut c_void);
            Some(value)
        }
    }

    fn check(
        &self,
        rc: c_int,
        what: impl FnOnce() -> String,
    ) -> Result<(), crate::PlayerError> {
        if rc >= 0 {
            Ok(())
        } else {
            Err(crate::PlayerError::Mpv(format!(
                "{}: {}",
                what(),
                self.error_string(rc)
            )))
        }
    }
}

pub fn to_cstring(s: &str) -> Result<CString, crate::PlayerError> {
    CString::new(s).map_err(|_| crate::PlayerError::Mpv(format!("string contains NUL: {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_constants_are_sane() {
        // Whatever build.rs resolved, the SW params must be distinct and in the
        // range mpv has ever used for them. Evaluated at compile time so a bad
        // generated layout fails the build, not just the test run.
        const { assert!(SW_SIZE == 17 || SW_SIZE == 18) };
        const { assert!(SW_FORMAT == SW_SIZE + 1) };
        const { assert!(SW_STRIDE == SW_SIZE + 2) };
        const { assert!(SW_POINTER == SW_SIZE + 3) };
        const { assert!(BLOCK_FOR_TARGET_TIME > 0 && BLOCK_FOR_TARGET_TIME < SW_SIZE) };
    }

    #[test]
    fn render_param_abi_matches_c() {
        // struct mpv_render_param { enum (int); void *data; }
        assert_eq!(std::mem::offset_of!(mpv_render_param, type_), 0);
        // Padded to the pointer alignment, exactly as the C compiler does.
        assert_eq!(std::mem::offset_of!(mpv_render_param, data), 8);
        assert_eq!(std::mem::size_of::<mpv_render_param>(), 16);
    }

    #[test]
    fn event_abi_matches_c() {
        // struct mpv_event { enum; int error; uint64_t reply_userdata; void *data; }
        assert_eq!(std::mem::offset_of!(mpv_event, event_id), 0);
        assert_eq!(std::mem::offset_of!(mpv_event, error), 4);
        assert_eq!(std::mem::offset_of!(mpv_event, reply_userdata), 8);
        assert_eq!(std::mem::offset_of!(mpv_event, data), 16);
        assert_eq!(std::mem::size_of::<mpv_event>(), 24);
    }

    #[test]
    fn event_property_abi_matches_c() {
        // struct mpv_event_property { const char *name; enum format; void *data; }
        assert_eq!(std::mem::offset_of!(mpv_event_property, name), 0);
        assert_eq!(std::mem::offset_of!(mpv_event_property, format), 8);
        assert_eq!(std::mem::offset_of!(mpv_event_property, data), 16);
    }

    #[test]
    fn event_end_file_abi_matches_c() {
        assert_eq!(std::mem::offset_of!(mpv_event_end_file, reason), 0);
        assert_eq!(std::mem::offset_of!(mpv_event_end_file, error), 4);
        assert_eq!(std::mem::offset_of!(mpv_event_end_file, playlist_entry_id), 8);
        assert_eq!(std::mem::offset_of!(mpv_event_end_file, playlist_insert_id), 16);
        assert_eq!(
            std::mem::offset_of!(mpv_event_end_file, playlist_insert_num_entries),
            24
        );
        assert_eq!(std::mem::size_of::<mpv_event_end_file>(), 32);
    }
}
