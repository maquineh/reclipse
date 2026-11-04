//! Diagnostic: load plugins and execute their first command.
//!
//! Usage:
//!   cargo run -p reclipse-plugin-host --example list_plugins -- target/debug

use std::path::Path;

use reclipse_plugin_api::InvokeContext;
use reclipse_plugin_host::PluginHost;

fn main() {
    let mut host = PluginHost::new();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() {
        host.load_default_paths();
    } else {
        for path in args {
            host.load_dir(Path::new(&path));
        }
    }

    println!("plugins: {:#?}", host.plugins());
    for error in host.load_errors() {
        eprintln!("load error: {error}");
    }

    let commands = host.commands();
    for command in &commands {
        println!(
            "command: plugin[{}] {} -> {} ({})",
            command.plugin_index, command.plugin_name, command.id, command.title
        );
    }

    if let Some(command) = commands.first() {
        let context = InvokeContext {
            active_file: std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string()),
            project_root: std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string()),
            selection: None,
        };
        match host.execute(command.plugin_index, &command.id, &context) {
            Ok(response) => println!("response: {response:#?}"),
            Err(error) => eprintln!("invoke error: {error:#}"),
        }
    }
}
