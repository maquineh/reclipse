//! One-click JDT Language Server packaging.
//!
//! The IDE does not bundle JDT LS (EPL-licensed, platform-specific). Instead,
//! **JDT: instalar servidor** downloads the latest GitHub release tarball
//! (`eclipse-jdtls/eclipse.jdt.ls`), extracts it under
//! `~/.reclipse/jdtls`, and writes a `jdtls` launcher script that the
//! existing stdio client (`jdtls.rs`) drives. Everything runs on explicit
//! user request; afterwards the server is used automatically unless
//! `RECLIPSE_JDTLS` points elsewhere.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const GITHUB_LATEST_RELEASE: &str =
    "https://api.github.com/repos/eclipse-jdtls/eclipse.jdt.ls/releases/latest";

pub fn default_install_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    {
        return Some(home.join(".reclipse").join("jdtls"));
    }
    None
}

/// Pick the JDT LS tarball from a GitHub releases API payload.
pub fn release_asset_url(release_json: &str) -> Option<String> {
    let payload: serde_json::Value = serde_json::from_str(release_json).ok()?;
    let assets = payload.get("assets")?.as_array()?;
    assets
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            let url = asset.get("browser_download_url")?.as_str()?;
            (name.contains("jdt-language-server") && name.ends_with(".tar.gz"))
                .then(|| url.to_owned())
        })
        .next()
}

/// Fetch plain text over HTTPS with `curl` (present on Windows 10+).
fn curl_text(url: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-sSL", "--max-time", "60", url])
        .output()
        .map_err(|error| format!("curl indisponível ({error}); instale o curl ou baixe manualmente de https://github.com/eclipse-jdtls/eclipse.jdt.ls/releases"))?;
    if !output.status.success() {
        return Err(format!(
            "curl falhou: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "resposta inválida (UTF-8) do GitHub".to_owned())
}

/// Resolve the latest JDT LS tarball URL via the GitHub releases API.
pub fn latest_asset_url() -> Result<String, String> {
    let json = curl_text(GITHUB_LATEST_RELEASE)?;
    release_asset_url(&json)
        .ok_or_else(|| "nenhum tarball jdt-language-server no último release".to_owned())
}

/// Locate an extracted JDT LS tree: the directory holding `plugins/` and a
/// `config_*` directory (tolerates one extra nesting level from tarballs).
pub fn find_server_home(dir: &Path) -> Option<PathBuf> {
    if is_server_home(dir) {
        return Some(dir.to_path_buf());
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() && is_server_home(&path) {
            return Some(path);
        }
    }
    None
}

fn is_server_home(dir: &Path) -> bool {
    if !dir.join("plugins").is_dir() {
        return false;
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_name().to_string_lossy().starts_with("config_") && entry.path().is_dir()
        })
}

fn launcher_jar(home: &Path) -> Option<PathBuf> {
    let mut jars: Vec<PathBuf> = std::fs::read_dir(home.join("plugins"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar")
                })
        })
        .collect();
    jars.sort();
    jars.pop()
}

fn config_dir(home: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "config_win"
    } else if cfg!(target_os = "macos") {
        "config_mac"
    } else {
        "config_linux"
    };
    let candidate = home.join(name);
    if candidate.is_dir() {
        return Some(candidate);
    }
    std::fs::read_dir(home)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config_"))
        })
}

/// Write the platform launcher consumed by `jdtls::spawn` (`-data` appended
/// by the client). Returns the launcher path.
pub fn write_launcher(home: &Path) -> Result<PathBuf, String> {
    let jar = launcher_jar(home)
        .ok_or_else(|| "equinox launcher não encontrado em plugins/".to_owned())?;
    let config = config_dir(home).ok_or_else(|| "config_* não encontrado".to_owned())?;
    if cfg!(windows) {
        let launcher = home.join("jdtls.cmd");
        let script = format!(
            "@echo off\r\njava -Declipse.application=org.eclipse.jdt.ls.core.id1 -Dosgi.bundles.defaultStartLevel=4 -Declipse.product=org.eclipse.jdt.ls.core.product -jar \"{}\" -configuration \"{}\" %*\r\n",
            jar.display(),
            config.display()
        );
        std::fs::write(&launcher, script)
            .map_err(|error| format!("falha ao gravar launcher: {error}"))?;
        Ok(launcher)
    } else {
        let launcher = home.join("jdtls");
        let script = format!(
            "#!/bin/sh\nexec java -Declipse.application=org.eclipse.jdt.ls.core.id1 -Dosgi.bundles.defaultStartLevel=4 -Declipse.product=org.eclipse.jdt.ls.core.product -jar \"{}\" -configuration \"{}\" \"$@\"\n",
            jar.display(),
            config.display()
        );
        std::fs::write(&launcher, script)
            .map_err(|error| format!("falha ao gravar launcher: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&launcher)
                .map_err(|error| format!("falha ao ler permissões: {error}"))?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&launcher, permissions)
                .map_err(|error| format!("falha em chmod +x: {error}"))?;
        }
        Ok(launcher)
    }
}

/// The launcher installed by [`install_from_url`], if present and valid.
pub fn installed_launcher() -> Option<PathBuf> {
    let home = default_install_dir()?;
    let server_home = find_server_home(&home)?;
    let launcher = if cfg!(windows) {
        server_home.join("jdtls.cmd")
    } else {
        server_home.join("jdtls")
    };
    launcher.is_file().then_some(launcher)
}

/// Download, extract and wire a JDT LS release tarball. Explicit, cancellable,
/// network-on-demand like every other fetch in the IDE.
pub fn install_from_url(
    url: &str,
    on_output: &mut dyn FnMut(&str),
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let dir = default_install_dir().ok_or_else(|| "diretório home não encontrado".to_owned())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("falha ao criar {dir:?}: {error}"))?;
    let archive = std::env::temp_dir().join(format!("jdtls-{}.tar.gz", std::process::id()));
    on_output(&format!("baixando {url} …"));
    let started = Instant::now();
    let mut child = Command::new("curl")
        .args([
            "-sSL",
            "--max-time",
            &timeout.as_secs().max(60).to_string(),
            "-o",
            &archive.display().to_string(),
            url,
        ])
        .spawn()
        .map_err(|error| format!("curl indisponível ({error})"))?;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Err("instalação do JDT LS cancelada".to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err("curl falhou ao baixar o JDT LS".to_owned());
                }
                break;
            }
            Ok(None) => {
                if started.elapsed() > timeout + Duration::from_secs(120) {
                    let _ = child.kill();
                    return Err("timeout no download do JDT LS".to_owned());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("falha ao aguardar curl: {error}")),
        }
    }
    on_output("extraindo …");
    // `tar` ships with Windows 10+ and every Unix.
    let status = Command::new("tar")
        .args([
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &dir.display().to_string(),
        ])
        .status()
        .map_err(|error| format!("tar indisponível ({error})"))?;
    let _ = std::fs::remove_file(&archive);
    if !status.success() {
        return Err("falha ao extrair o tarball do JDT LS".to_owned());
    }
    let home = find_server_home(&dir).ok_or_else(|| "layout inesperado no tarball".to_owned())?;
    let launcher = write_launcher(&home)?;
    on_output(&format!("JDT LS instalado em {}", home.display()));
    Ok(launcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_tarball_from_release_api() {
        let json = r#"{"assets": [
            {"name": "jdt-language-server-1.44.0-202503271lable.tar.gz", "browser_download_url": "https://example.invalid/jdt.tar.gz"},
            {"name": "checksums.txt", "browser_download_url": "https://example.invalid/sums"}
        ]}"#;
        assert_eq!(
            release_asset_url(json).as_deref(),
            Some("https://example.invalid/jdt.tar.gz")
        );
        assert!(release_asset_url(r#"{"assets": []}"#).is_none());
        assert!(release_asset_url("not json").is_none());
    }

    #[test]
    fn detects_server_home_and_writes_launcher() {
        let dir = tempfile::tempdir().unwrap();
        // Tarballs often nest one level.
        let home = dir.path().join("jdt-language-server-1.44.0");
        std::fs::create_dir_all(home.join("plugins")).unwrap();
        std::fs::create_dir_all(home.join("config_win")).unwrap();
        std::fs::write(
            home.join("plugins/org.eclipse.equinox.launcher_1.7.0.jar"),
            b"jar",
        )
        .unwrap();
        assert_eq!(
            find_server_home(dir.path()).as_deref(),
            Some(home.as_path())
        );
        let launcher = write_launcher(&home).unwrap();
        assert!(launcher.is_file());
        let script = std::fs::read_to_string(&launcher).unwrap();
        assert!(script.contains("org.eclipse.equinox.launcher_1.7.0.jar"));
        assert!(script.contains("config_win"));
    }

    #[test]
    fn install_dir_is_stable() {
        // Deterministic regardless of machine: uses a fake HOME.
        std::env::set_var("RECLIPSE_TEST_HOME", "x");
        std::env::remove_var("RECLIPSE_TEST_HOME");
        assert!(default_install_dir().is_some());
    }
}
