//! Stable C ABI shared by Reclipse and native Rust plugins.
//!
//! The ABI is versioned and data-oriented: commands are JSON strings, not Rust
//! trait objects. That keeps plugins decoupled from the compiler's unstable
//! Rust ABI and lets a plugin be written in any language that can export C
//! functions later.
//!
//! String ownership rules:
//! - Strings returned by a plugin are allocated by the plugin and must be
//!   released with the plugin's `free_string`.
//! - Strings returned by the host (future host services) are released with
//!   `HostApiV1::free_string`.

use std::ffi::{c_char, CStr, CString};

use serde::{Deserialize, Serialize};

pub const ABI_VERSION: u32 = 1;

/// Services the host exposes to a plugin. New services are added by creating a
/// `HostApiV2`; existing function pointers never change meaning within a major
/// ABI version.
#[repr(C)]
pub struct HostApiV1 {
    pub abi_version: u32,
    /// Write a UTF-8 line to the host log. The host copies the string.
    pub log: unsafe extern "C" fn(*const c_char),
    /// Free a string previously returned by the host.
    pub free_string: unsafe extern "C" fn(*mut c_char),
}

/// Entry point descriptor returned by `reclipse_plugin_entry_v1`.
#[repr(C)]
pub struct PluginV1 {
    pub abi_version: u32,
    pub id: *const c_char,
    pub name: *const c_char,
    pub version: *const c_char,
    /// Called once after loading. Return 0 on success.
    pub init: Option<unsafe extern "C" fn(*const HostApiV1) -> i32>,
    /// Called before the library is unloaded.
    pub shutdown: Option<unsafe extern "C" fn()>,
    /// Execute `command` with a JSON context. Returns a JSON `PluginResponse`.
    pub invoke: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char>,
    /// Free a string returned by `invoke`.
    pub free_string: Option<unsafe extern "C" fn(*mut c_char)>,
}

pub type PluginEntryFn = unsafe extern "C" fn() -> *const PluginV1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandInfo {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InvokeContext {
    pub active_file: Option<String>,
    pub project_root: Option<String>,
    pub selection: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginResponse {
    /// Shown in the status bar / notification area.
    pub message: Option<String>,
    /// Inserted at the cursor (or replacing the selection when
    /// `replace_selection` is true).
    pub insert_text: Option<String>,
    pub replace_selection: bool,
    /// Optional navigation request.
    pub open_file: Option<String>,
    pub line: Option<usize>,
}

/// Read a borrowed C string into an owned Rust string.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated string.
pub unsafe fn read_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}

/// Allocate a string for the other side of the ABI. The receiver must call the
/// matching `free_string`.
pub fn to_c_string(value: &str) -> *mut c_char {
    let sanitized = value.replace('\0', "");
    match CString::new(sanitized) {
        Ok(value) => value.into_raw(),
        Err(_) => CString::new("{}")
            .expect("static string has no NUL")
            .into_raw(),
    }
}

/// Take ownership of a string returned across the ABI.
///
/// # Safety
/// `ptr` must have been allocated by a matching `CString::into_raw` and not
/// freed yet.
pub unsafe fn take_c_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        let value = CString::from_raw(ptr);
        Some(value.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_c_strings() {
        let ptr = to_c_string("olá \u{1f600}");
        let value = unsafe { take_c_string(ptr) }.unwrap();
        assert_eq!(value, "olá \u{1f600}");
    }

    #[test]
    fn serializes_command_protocol() {
        let list = CommandList {
            commands: vec![CommandInfo {
                id: "x".into(),
                title: "X".into(),
            }],
        };
        let json = serde_json::to_string(&list).unwrap();
        let back: CommandList = serde_json::from_str(&json).unwrap();
        assert_eq!(back.commands.len(), 1);
    }
}
