//! Minimal CoreFoundation bindings shared by the macOS adapters.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFURLRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type Boolean = u8;

const K_CF_NUMBER_SINT64_TYPE: CFIndex = 4;
pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFAllocatorRef,
        buffer: *const u8,
        len: CFIndex,
        is_directory: Boolean,
    ) -> CFURLRef;
    fn CFURLGetFileSystemRepresentation(url: CFURLRef, resolve: Boolean, buffer: *mut u8, max: CFIndex) -> Boolean;
    fn CFURLGetTypeID() -> CFTypeID;
    fn CFURLCopyResourcePropertyForKey(url: CFURLRef, key: CFStringRef, value: *mut CFTypeRef, error: *mut CFTypeRef) -> Boolean;
    pub fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFUUIDGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFNumberGetValue(number: CFTypeRef, the_type: CFIndex, value_ptr: *mut c_void) -> Boolean;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> Boolean;
    fn CFStringGetCString(s: CFStringRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
    pub fn CFStringCreateWithCString(alloc: CFAllocatorRef, c_str: *const c_char, encoding: u32) -> CFStringRef;
    fn CFUUIDCreateString(alloc: CFAllocatorRef, uuid: CFTypeRef) -> CFStringRef;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
}

/// A CoreFoundation object this code owns (released on drop).
pub struct Owned(pub CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

impl Owned {
    /// Takes ownership of a "Create"/"Copy" result; `None` for NULL.
    pub fn new(r: CFTypeRef) -> Option<Owned> {
        // Not `then_some`: that would build (and drop, so release) an Owned for NULL too.
        if r.is_null() { None } else { Some(Owned(r)) }
    }
}

/// A CFString from Rust text, owned.
pub fn cfstr(s: &str) -> Owned {
    let c = std::ffi::CString::new(s).expect("no NUL in keys");
    Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) })
}

/// `v` as text: a CFString, or a CFUUID in its canonical form.
pub fn string_of(v: CFTypeRef) -> Option<String> {
    if v.is_null() {
        return None;
    }
    unsafe {
        let t = CFGetTypeID(v);
        if t == CFUUIDGetTypeID() {
            let s = Owned::new(CFUUIDCreateString(std::ptr::null(), v))?;
            return string_of(s.0);
        }
        if t != CFStringGetTypeID() {
            return None;
        }
        let mut buf = [0 as c_char; 1024];
        if CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as CFIndex, K_CF_STRING_ENCODING_UTF8) == 0 {
            return None;
        }
        let s = CStr::from_ptr(buf.as_ptr()).to_string_lossy().trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

pub fn number_of(v: CFTypeRef) -> Option<u64> {
    if v.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(v) != CFNumberGetTypeID() {
            return None;
        }
        let mut out: i64 = 0;
        (CFNumberGetValue(v, K_CF_NUMBER_SINT64_TYPE, (&mut out as *mut i64).cast()) != 0 && out >= 0).then_some(out as u64)
    }
}

pub fn bool_of(v: CFTypeRef) -> Option<bool> {
    if v.is_null() {
        return None;
    }
    unsafe { (CFGetTypeID(v) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(v) != 0) }
}

/// The value under `key` in a CFDictionary (borrowed, not owned).
pub fn dict_get(dict: CFDictionaryRef, key: CFStringRef) -> CFTypeRef {
    if dict.is_null() {
        return std::ptr::null();
    }
    unsafe {
        if CFGetTypeID(dict) != CFDictionaryGetTypeID() {
            return std::ptr::null();
        }
        CFDictionaryGetValue(dict, key)
    }
}

/// The file-system path of a CFURL value.
pub fn path_of_url(v: CFTypeRef) -> Option<std::path::PathBuf> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFURLGetTypeID() } {
        return None;
    }
    let mut buf = vec![0u8; 4096];
    let ok = unsafe { CFURLGetFileSystemRepresentation(v, 1, buf.as_mut_ptr(), buf.len() as CFIndex) };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
}

/// A file URL for `path`.
pub struct Url(pub Owned);

impl Url {
    pub fn new(path: &Path, is_dir: bool) -> Option<Url> {
        let bytes = path.as_os_str().as_bytes();
        let r = unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex, is_dir as Boolean) };
        Owned::new(r).map(Url)
    }

    fn copy(&self, key: CFStringRef) -> Option<Owned> {
        let mut value: CFTypeRef = std::ptr::null();
        let ok = unsafe { CFURLCopyResourcePropertyForKey(self.0.0, key, &mut value, std::ptr::null_mut()) };
        if ok == 0 { None } else { Owned::new(value) }
    }

    pub fn number(&self, key: CFStringRef) -> Option<u64> {
        number_of(self.copy(key)?.0)
    }

    pub fn string(&self, key: CFStringRef) -> Option<String> {
        string_of(self.copy(key)?.0)
    }
}
