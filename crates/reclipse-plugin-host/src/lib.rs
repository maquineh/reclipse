//! Loads versioned native plugins and brokers JSON commands.

use std::env;
use std::ffi::{c_char, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libloading::Library;
use reclipse_plugin_api::{
    read_c_string, CommandList, HostApiV1, InvokeContext, PluginEntryFn, PluginResponse,
    ABI_VERSION,
};

pub const ENTRY_SYMBOL: &[u8] = b"reclipse_plugin_entry_v1\0";

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct PluginCommand {
    pub plugin_index: usize,
    pub plugin_name: String,
    pub id: String,
    pub title: String,
}

struct NativePlugin {
    info: PluginInfo,
    shutdown: Option<unsafe extern "C" fn()>,
    invoke: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char,
    free_string: unsafe extern "C" fn(*mut c_char),
    /// Keeps the dynamic library mapped for as long as any function pointer
    /// above can be called. Declared last so it is dropped after the pointers.
    _library: Library,
}

impl Drop for NativePlugin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown {
            // The plugin ABI requires shutdown to be panic-free.
            unsafe { shutdown() };
        }
    }
}

/// Owns loaded plugins and the host API passed to them.
///
/// Must be dropped on the same thread that loaded the plugins.
pub struct PluginHost {
    plugins: Vec<NativePlugin>,
    api: Box<HostApiV1>,
    search_paths: Vec<PathBuf>,
    load_errors: Vec<String>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    pub fn new() -> Self {
        let api = Box::new(HostApiV1 {
            abi_version: ABI_VERSION,
            log: host_log,
            free_string: host_free_string,
        });
        Self {
            plugins: Vec::new(),
            api,
            search_paths: Vec::new(),
            load_errors: Vec::new(),
        }
    }

    /// Load plugins from the standard locations. Missing directories are
    /// ignored; malformed plugins are reported in [`Self::load_errors`].
    pub fn load_default_paths(&mut self) {
        for dir in default_search_paths() {
            self.load_dir(&dir);
        }
    }

    pub fn load_dir(&mut self, dir: &Path) {
        if !dir.is_dir() {
            return;
        }
        if !self.search_paths.iter().any(|known| known == dir) {
            self.search_paths.push(dir.to_path_buf());
        }

        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_plugin_library(path))
            .collect();
        candidates.sort();

        for path in candidates {
            if let Err(error) = self.load_file(&path) {
                let message = format!("{}: {error:#}", path.display());
                eprintln!("[reclipse] plugin load failed: {message}");
                self.load_errors.push(message);
            }
        }
    }

    fn load_file(&mut self, path: &Path) -> Result<()> {
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load dynamic library {}", path.display()))?;

        let entry: libloading::Symbol<'_, PluginEntryFn> = unsafe { library.get(ENTRY_SYMBOL) }
            .with_context(|| format!("missing reclipse_plugin_entry_v1 in {}", path.display()))?;

        let descriptor_ptr = unsafe { entry() };
        if descriptor_ptr.is_null() {
            bail!("plugin entry returned a null descriptor");
        }
        let descriptor = unsafe { &*descriptor_ptr };
        if descriptor.abi_version != ABI_VERSION {
            bail!(
                "unsupported plugin ABI {} (host supports {})",
                descriptor.abi_version,
                ABI_VERSION
            );
        }

        let id = unsafe { read_c_string(descriptor.id) }.context("plugin has no id")?;
        let name = unsafe { read_c_string(descriptor.name) }.unwrap_or_else(|| id.clone());
        let version =
            unsafe { read_c_string(descriptor.version) }.unwrap_or_else(|| "0.0.0".into());
        let invoke = descriptor
            .invoke
            .context("plugin does not export an invoke function")?;
        let free_string = descriptor
            .free_string
            .context("plugin does not export free_string")?;

        if self.plugins.iter().any(|plugin| plugin.info.id == id) {
            return Ok(());
        }

        if let Some(init) = descriptor.init {
            let code = unsafe { init(&*self.api) };
            if code != 0 {
                bail!("plugin init returned error code {code}");
            }
        }

        self.plugins.push(NativePlugin {
            info: PluginInfo { id, name, version },
            _library: library,
            shutdown: descriptor.shutdown,
            invoke,
            free_string,
        });
        Ok(())
    }

    pub fn reload(&mut self) {
        *self = Self::new();
        self.load_default_paths();
    }

    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|plugin| plugin.info.clone())
            .collect()
    }

    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    pub fn search_paths(&self) -> &[PathBuf] {
        &self.search_paths
    }

    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }

    /// List every command advertised by loaded plugins.
    pub fn commands(&self) -> Vec<PluginCommand> {
        let mut commands = Vec::new();
        for (plugin_index, plugin) in self.plugins.iter().enumerate() {
            match call_plugin(plugin, "list_commands", "{}") {
                Ok(json) => match serde_json::from_str::<CommandList>(&json) {
                    Ok(list) => {
                        for command in list.commands {
                            commands.push(PluginCommand {
                                plugin_index,
                                plugin_name: plugin.info.name.clone(),
                                id: command.id,
                                title: command.title,
                            });
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "[reclipse] plugin {} returned invalid command list: {error}",
                            plugin.info.id
                        );
                    }
                },
                Err(error) => {
                    eprintln!(
                        "[reclipse] plugin {} failed to list commands: {error:#}",
                        plugin.info.id
                    );
                }
            }
        }
        commands
    }

    pub fn execute(
        &self,
        plugin_index: usize,
        command: &str,
        context: &InvokeContext,
    ) -> Result<PluginResponse> {
        let plugin = self
            .plugins
            .get(plugin_index)
            .with_context(|| format!("unknown plugin index {plugin_index}"))?;
        let context = serde_json::to_string(context)?;
        let response = call_plugin(plugin, command, &context)?;
        serde_json::from_str(&response)
            .with_context(|| format!("plugin {} returned invalid JSON", plugin.info.id))
    }
}

fn call_plugin(plugin: &NativePlugin, command: &str, context: &str) -> Result<String> {
    let command = CString::new(command).context("command contains NUL")?;
    let context = CString::new(context).context("context contains NUL")?;
    let response_ptr = unsafe { (plugin.invoke)(command.as_ptr(), context.as_ptr()) };
    if response_ptr.is_null() {
        bail!("plugin returned a null response");
    }
    let response = unsafe { CStr::from_ptr(response_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { (plugin.free_string)(response_ptr) };
    Ok(response)
}

fn is_plugin_library(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.starts_with("reclipse_plugin_") {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extension.eq_ignore_ascii_case("dll")
                || extension.eq_ignore_ascii_case("so")
                || extension.eq_ignore_ascii_case("dylib")
        })
        .unwrap_or(false)
}

pub fn default_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(value) = env::var("RECLIPSE_PLUGIN_PATH") {
        for part in value.split(';') {
            if !part.trim().is_empty() {
                dirs.push(PathBuf::from(part.trim()));
            }
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.join("plugins"));
            dirs.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = env::current_dir() {
        dirs.push(cwd.join("plugins"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

unsafe extern "C" fn host_log(message: *const c_char) {
    if let Some(message) = unsafe { read_c_string(message) } {
        eprintln!("[reclipse plugin] {message}");
    }
}

unsafe extern "C" fn host_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_starts_without_plugins() {
        let host = PluginHost::new();
        assert_eq!(host.plugin_count(), 0);
        assert!(host.commands().is_empty());
    }

    #[test]
    fn default_paths_include_executable_dir() {
        let paths = default_search_paths();
        assert!(!paths.is_empty());
    }
}
