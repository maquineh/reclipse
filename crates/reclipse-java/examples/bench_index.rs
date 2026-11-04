//! Measure project discovery + syntax indexing.
//!
//! Usage:
//!   cargo run --release -p reclipse-java --example bench_index -- <path>

use std::path::Path;
use std::time::Instant;

use reclipse_core::discover_project;
use reclipse_java::build_index;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: bench_index <project-root>");
        std::process::exit(2);
    };

    let start = Instant::now();
    let project = match discover_project(Path::new(&path)) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("discovery failed: {error:#}");
            std::process::exit(1);
        }
    };
    let discovery = start.elapsed();

    let start = Instant::now();
    let index = build_index(project.clone(), None);
    let indexing = start.elapsed();

    println!(
        "project={} build={:?} files={} java_files={} symbols={} problems={} discovery_ms={} index_ms={}",
        project.name,
        project.build_system,
        project.all_files.len(),
        project.java_files.len(),
        index.stats.symbols,
        index.stats.problems,
        discovery.as_millis(),
        indexing.as_millis(),
    );
}
