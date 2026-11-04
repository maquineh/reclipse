//! Example out-of-process UI plugin for Reclipse.
//!
//! Build with `cargo build -p reclipse-plugin-hello`. The resulting
//! `reclipse_plugin_hello.dll` is discovered automatically from the directory
//! containing `reclipse.exe` (or from `RECLIPSE_PLUGIN_PATH`).

use std::ffi::{c_char, CString};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use reclipse_plugin_api::{
    read_c_string, to_c_string, CommandInfo, CommandList, HostApiV1, InvokeContext, PluginResponse,
    PluginV1, ABI_VERSION,
};

const PLUGIN_ID: &[u8] = b"reclipse.hello\0";
const PLUGIN_NAME: &[u8] = b"Hello Plugin\0";
const PLUGIN_VERSION: &[u8] = b"0.1.0\0";

static HOST_API: AtomicPtr<HostApiV1> = AtomicPtr::new(std::ptr::null_mut());

struct Descriptor(PluginV1);
// The descriptor only contains immutable pointers to static data and function
// pointers. Sharing it with the host is safe; the host never mutates it.
unsafe impl Sync for Descriptor {}

static DESCRIPTOR: Descriptor = Descriptor(PluginV1 {
    abi_version: ABI_VERSION,
    id: PLUGIN_ID.as_ptr() as *const c_char,
    name: PLUGIN_NAME.as_ptr() as *const c_char,
    version: PLUGIN_VERSION.as_ptr() as *const c_char,
    init: Some(init),
    shutdown: Some(shutdown),
    invoke: Some(invoke),
    free_string: Some(free_string),
});

#[no_mangle]
pub extern "C" fn reclipse_plugin_entry_v1() -> *const PluginV1 {
    &DESCRIPTOR.0
}

unsafe extern "C" fn init(api: *const HostApiV1) -> i32 {
    if api.is_null() {
        return 1;
    }
    let api_ref = unsafe { &*api };
    if api_ref.abi_version != ABI_VERSION {
        return 2;
    }
    HOST_API.store(api as *mut HostApiV1, Ordering::SeqCst);
    log("hello-plugin initialized");
    0
}

unsafe extern "C" fn shutdown() {
    log("hello-plugin shutting down");
    HOST_API.store(std::ptr::null_mut(), Ordering::SeqCst);
}

unsafe extern "C" fn invoke(command: *const c_char, args: *const c_char) -> *mut c_char {
    let command = unsafe { read_c_string(command) }.unwrap_or_default();
    let args = unsafe { read_c_string(args) }.unwrap_or_default();

    let json = if command == "list_commands" {
        serde_json::to_string(&commands()).unwrap_or_else(|_| "{\"commands\":[]}".to_owned())
    } else {
        let response = handle_command(&command, &args);
        serde_json::to_string(&response)
            .unwrap_or_else(|_| "{\"message\":\"serialization error\"}".to_owned())
    };
    to_c_string(&json)
}

unsafe extern "C" fn free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

fn commands() -> CommandList {
    CommandList {
        commands: vec![
            CommandInfo {
                id: "hello.insert_timestamp".to_owned(),
                title: "Hello: inserir timestamp".to_owned(),
            },
            CommandInfo {
                id: "hello.file_stats".to_owned(),
                title: "Hello: estatísticas do arquivo ativo".to_owned(),
            },
        ],
    }
}

fn handle_command(command: &str, args: &str) -> PluginResponse {
    match command {
        "hello.insert_timestamp" => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            PluginResponse {
                message: Some("Timestamp inserido pelo hello-plugin".to_owned()),
                insert_text: Some(format!("// TODO(reclipse): revisar (unix {seconds})\n")),
                ..Default::default()
            }
        }
        "hello.file_stats" => {
            let context: InvokeContext = serde_json::from_str(args).unwrap_or_default();
            match context.active_file {
                Some(file) => match std::fs::read_to_string(&file) {
                    Ok(text) => {
                        let lines = text.lines().count();
                        let bytes = text.len();
                        let classes = text.matches("class ").count();
                        PluginResponse {
                            message: Some(format!(
                                "{file}: {lines} linhas, {bytes} bytes, {classes} declarações 'class'"
                            )),
                            ..Default::default()
                        }
                    }
                    Err(error) => PluginResponse {
                        message: Some(format!("não foi possível ler {file}: {error}")),
                        ..Default::default()
                    },
                },
                None => PluginResponse {
                    message: Some("nenhum arquivo ativo".to_owned()),
                    ..Default::default()
                },
            }
        }
        other => PluginResponse {
            message: Some(format!("comando desconhecido: {other}")),
            ..Default::default()
        },
    }
}

fn log(message: &str) {
    let api = HOST_API.load(Ordering::SeqCst);
    if api.is_null() {
        return;
    }
    if let Ok(message) = CString::new(message.replace('\0', "")) {
        unsafe { ((*api).log)(message.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_commands() {
        let list = commands();
        assert_eq!(list.commands.len(), 2);
        assert!(list
            .commands
            .iter()
            .any(|command| command.id == "hello.file_stats"));
    }

    #[test]
    fn timestamp_command_returns_insertable_text() {
        let response = handle_command("hello.insert_timestamp", "{}");
        assert!(response.insert_text.unwrap().contains("TODO(reclipse)"));
    }

    #[test]
    fn unknown_command_is_reported() {
        let response = handle_command("nope", "{}");
        assert!(response.message.unwrap().contains("desconhecido"));
    }
}
