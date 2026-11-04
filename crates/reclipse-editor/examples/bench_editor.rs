//! Synthetic editor benchmark for M1.
//!
//! Run with:
//!   cargo run --release -p reclipse-editor --example bench_editor

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use reclipse_editor::{tokenize_line, Document, HighlightState};

fn main() {
    let root = std::env::temp_dir().join(format!("reclipse-editor-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let big_path = root.join("Big.java");
    let big_source = generate_java(100_000);
    fs::write(&big_path, &big_source).unwrap();

    let start = Instant::now();
    let mut big = Document::open(&big_path).unwrap();
    let open_ms = start.elapsed().as_millis();

    let start = Instant::now();
    let mut state = HighlightState::Normal;
    for line in 0..big.line_count() {
        let text = big.line_text(line);
        let _ = tokenize_line(&text, &mut state);
    }
    let lex_ms = start.elapsed().as_millis();

    let start = Instant::now();
    big.set_cursor(big.len_chars(), false);
    for index in 0..1_000 {
        big.insert(if index % 7 == 0 { " " } else { "x" });
    }
    let edits_ms = start.elapsed().as_millis();

    let mut tab_paths = Vec::with_capacity(100);
    for index in 0..100 {
        let path = root.join(format!("Tab{index}.java"));
        fs::write(&path, generate_java(1_000)).unwrap();
        tab_paths.push(path);
    }
    let start = Instant::now();
    let mut bytes = 0_usize;
    let mut tabs = Vec::with_capacity(100);
    for path in &tab_paths {
        let document = Document::open(path).unwrap();
        bytes += document.rope.len_bytes();
        tabs.push(document);
    }
    let open_tabs_ms = start.elapsed().as_millis();

    println!(
        "100k_lines: open={open_ms}ms lex={lex_ms}ms edits_1000={edits_ms}ms | \
         100_tabs: open={open_tabs_ms}ms bytes={bytes} loaded={}",
        tabs.len()
    );

    drop(tabs);
    drop(big);
    let _ = fs::remove_dir_all(&root);
    let _ = PathBuf::from("");
}

fn generate_java(lines: usize) -> String {
    let mut source = String::with_capacity(lines * 48);
    source.push_str("package bench;\n\npublic class Big {\n");
    for line in 0..lines {
        source.push_str("    int value");
        source.push_str(&line.to_string());
        source.push_str(" = ");
        source.push_str(&(line % 97).to_string());
        source.push_str("; // benchmark\n");
    }
    source.push_str("}\n");
    source
}
