//! Inspect the local JDK classpath index.
//!
//! cargo run --release -p reclipse-classpath --example index_jdk

use reclipse_classpath::{build_index, find_jdk, ClasspathResolution};

fn main() {
    let Some(jdk) = find_jdk() else {
        eprintln!("no JDK found");
        return;
    };
    println!(
        "jdk={} version={} jmods={} rt_jar={:?}",
        jdk.home.display(),
        jdk.version,
        jdk.jmods.len(),
        jdk.rt_jar
    );
    let resolution = ClasspathResolution {
        sources: Vec::new(),
        jars: Vec::new(),
        jdk: Some(jdk),
        jdk_target: None,
        warnings: Vec::new(),
        build_system: reclipse_core::BuildSystem::Plain,
    };
    let index = build_index(&resolution, None);
    println!(
        "archives={} classes={} types={} elapsed_ms={} warnings={}",
        index.stats.archives,
        index.stats.classes,
        index.type_names.len(),
        index.stats.elapsed_ms,
        index.warnings.len()
    );
    println!(
        "java.lang.String: {:?}",
        index.get("java.lang.String").map(|class| &class.kind)
    );
    if let Some(class) = index.get("java.lang.String") {
        println!(
            "String methods sample: {:?}",
            class
                .methods
                .iter()
                .take(5)
                .map(|method| &method.signature)
                .collect::<Vec<_>>()
        );
    }
}
