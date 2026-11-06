//! Eclipse-style Java Build Path dialog (WTP-like "Build Path" menu).
//!
//! Reads and writes the Eclipse `.classpath` file. Maven/Gradle projects are
//! shown read-only (their classpath is managed by `pom.xml`/`build.gradle`).

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};

use crate::theme;

pub const JRE_CONTAINER: &str = "org.eclipse.jdt.launching.JRE_CONTAINER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Src,
    Lib,
    Con,
    Output,
    Other(String),
}

impl EntryKind {
    pub fn label(&self) -> &str {
        match self {
            Self::Src => "src",
            Self::Lib => "lib",
            Self::Con => "con",
            Self::Output => "output",
            Self::Other(kind) => kind,
        }
    }

    fn parse(kind: &str) -> Self {
        match kind {
            "src" => Self::Src,
            "lib" => Self::Lib,
            "con" => Self::Con,
            "output" => Self::Output,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClasspathEntry {
    pub kind: EntryKind,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct ClasspathModel {
    /// Entries shown (and editable) in the dialog.
    pub entries: Vec<ClasspathEntry>,
    /// Raw lines preserved verbatim on save (unknown tags, comments).
    pub other_lines: Vec<String>,
}

pub fn parse_classpath(text: &str) -> ClasspathModel {
    let mut model = ClasspathModel::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<classpathentry") {
            let kind = xml_attr(trimmed, "kind").unwrap_or_default();
            let path = xml_attr(trimmed, "path").unwrap_or_default();
            // `kind="var"` and unknown entries are kept read-only.
            match kind.as_str() {
                "src" | "lib" | "con" | "output" => model.entries.push(ClasspathEntry {
                    kind: EntryKind::parse(&kind),
                    path,
                }),
                _ => model.other_lines.push(line.to_owned()),
            }
        } else if trimmed.starts_with('<')
            && !trimmed.starts_with("<?")
            && !trimmed.starts_with("<classpath")
            && !trimmed.starts_with("</classpath")
        {
            model.other_lines.push(line.to_owned());
        }
    }
    model
}

pub fn serialize_classpath(model: &ClasspathModel) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<classpath>\n");
    for entry in &model.entries {
        out.push_str(&format!(
            "\t<classpathentry kind=\"{}\" path=\"{}\"/>\n",
            entry.kind.label(),
            entry.path,
        ));
    }
    for line in &model.other_lines {
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("</classpath>\n");
    out
}

fn xml_attr(line: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{name}={quote}");
        if let Some(start) = line.find(&pattern) {
            let value_start = start + pattern.len();
            if let Some(length) = line[value_start..].find(quote) {
                return Some(line[value_start..value_start + length].to_owned());
            }
        }
    }
    None
}

/// Editable state of the Configure Build Path dialog.
pub struct BuildPathDialog {
    pub root: PathBuf,
    /// `Some("pom.xml")` / `Some("build.gradle")` when the project is
    /// managed by Maven/Gradle: entries are shown read-only.
    pub manifest: Option<String>,
    pub model: ClasspathModel,
    pub new_src: String,
    pub new_lib: String,
    pub new_output: String,
    pub new_container: String,
    pub error: Option<String>,
    /// Resolved entries for read-only display (sources, jars).
    pub resolved_sources: Vec<String>,
    pub resolved_jars: Vec<String>,
}

pub enum DialogAction {
    None,
    Save,
    Close,
    OpenManifest,
}

impl BuildPathDialog {
    pub fn open(
        root: PathBuf,
        build_system: reclipse_core::BuildSystem,
        manifest: Option<String>,
        resolved_sources: Vec<String>,
        resolved_jars: Vec<String>,
    ) -> Self {
        let text = std::fs::read_to_string(root.join(".classpath")).unwrap_or_default();
        let model = parse_classpath(&text);
        let output = model
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Output)
            .map(|entry| entry.path.clone())
            .unwrap_or_default();
        Self {
            root,
            manifest: match build_system {
                reclipse_core::BuildSystem::Maven => Some("pom.xml".to_owned()),
                reclipse_core::BuildSystem::Gradle => {
                    Some(manifest.unwrap_or_else(|| "build.gradle".to_owned()))
                }
                _ => None,
            },
            model,
            new_src: String::new(),
            new_lib: String::new(),
            new_output: output,
            new_container: String::new(),
            error: None,
            resolved_sources,
            resolved_jars,
        }
    }

    fn is_managed(&self) -> bool {
        self.manifest.is_some()
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message);
    }

    fn add_src(&mut self) {
        let path = self.new_src.trim().replace('\\', "/");
        if path.is_empty() {
            self.fail("Informe a pasta fonte (ex.: src/main/java).".to_owned());
            return;
        }
        if path.starts_with('/') || path.starts_with("..") || Path::new(&path).is_absolute() {
            self.fail("Pasta fonte deve ser relativa ao projeto.".to_owned());
            return;
        }
        if self
            .model
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Src && entry.path == path)
        {
            self.fail("Pasta fonte já está no build path.".to_owned());
            return;
        }
        let candidate = self.root.join(&path);
        if !candidate.is_dir() {
            // WTP "New Source Folder" creates the folder: same here, it is an
            // explicit user action from the dialog.
            if let Err(error) = std::fs::create_dir_all(&candidate) {
                self.fail(format!("Não foi possível criar {path}: {error}"));
                return;
            }
        }
        self.model.entries.push(ClasspathEntry {
            kind: EntryKind::Src,
            path,
        });
        self.new_src.clear();
        self.error = None;
    }

    fn pick_jar(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Adicionar JAR ao Build Path")
            .add_filter("Java archives", &["jar"])
            .set_directory(&self.root)
            .pick_file()
        else {
            return;
        };
        match path.strip_prefix(&self.root) {
            Ok(relative) => {
                self.new_lib = relative.display().to_string().replace('\\', "/");
                self.add_lib();
            }
            Err(_) => {
                self.fail("JAR fora do projeto — copie para lib/ antes de adicionar.".to_owned())
            }
        }
    }

    fn add_lib(&mut self) {
        let path = self.new_lib.trim().replace('\\', "/");
        if path.is_empty() {
            self.fail("Informe o JAR (ex.: lib/dep.jar).".to_owned());
            return;
        }
        if Path::new(&path).is_absolute() {
            self.fail("Use caminho relativo ao projeto (ex.: lib/dep.jar).".to_owned());
            return;
        }
        if self
            .model
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Lib && entry.path == path)
        {
            self.fail("JAR já está no build path.".to_owned());
            return;
        }
        let candidate = self.root.join(&path);
        if candidate.is_file()
            || (candidate.is_dir()
                && candidate
                    .extension()
                    .is_none_or(|extension| extension != "jar"))
        {
            self.model.entries.push(ClasspathEntry {
                kind: EntryKind::Lib,
                path,
            });
            self.new_lib.clear();
            self.error = None;
        } else {
            self.fail(format!("JAR não encontrado: {path}"));
        }
    }

    fn add_container(&mut self) {
        let path = self.new_container.trim().to_owned();
        if path.is_empty() {
            self.fail("Informe o container (ex.: JRE_CONTAINER).".to_owned());
            return;
        }
        if self
            .model
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Con && entry.path == path)
        {
            self.fail("Container já está no build path.".to_owned());
            return;
        }
        self.model.entries.push(ClasspathEntry {
            kind: EntryKind::Con,
            path,
        });
        self.new_container.clear();
        self.error = None;
    }

    fn apply_output(&mut self) {
        let output = self.new_output.trim().to_owned();
        self.model
            .entries
            .retain(|entry| entry.kind != EntryKind::Output);
        if !output.is_empty() {
            self.model.entries.push(ClasspathEntry {
                kind: EntryKind::Output,
                path: output,
            });
        }
        self.error = None;
    }

    pub fn show(&mut self, ui: &mut Ui, jdk_info: Option<&str>) -> DialogAction {
        let mut action = DialogAction::None;
        if self.is_managed() {
            let manifest = self.manifest.clone().unwrap_or_default();
            ui.label(
                RichText::new(format!(
                    "Build path gerenciado por {manifest} — edite o manifesto. Abaixo, o estado resolvido (somente leitura)."
                ))
                .small()
                .color(theme::muted()),
            );
            if ui.small_button(format!("Abrir {manifest}")).clicked() {
                action = DialogAction::OpenManifest;
            }
            ui.separator();
            ui.label(RichText::new("Source folders").strong());
            if self.resolved_sources.is_empty() {
                ui.label(RichText::new("(nenhuma)").small().color(theme::muted()));
            }
            for source in &self.resolved_sources {
                ui.label(RichText::new(source).monospace().size(12.0));
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Libraries").strong());
            if self.resolved_jars.is_empty() {
                ui.label(RichText::new("(nenhuma)").small().color(theme::muted()));
            }
            for jar in self.resolved_jars.iter().take(200) {
                ui.label(RichText::new(jar).monospace().size(12.0));
            }
            if self.resolved_jars.len() > 200 {
                ui.label(
                    RichText::new(format!("… e outros {}", self.resolved_jars.len() - 200))
                        .small()
                        .color(theme::muted()),
                );
            }
            return action;
        }

        if let Some(jdk) = jdk_info {
            ui.label(RichText::new(jdk).small().color(theme::muted()));
        }
        if let Some(error) = self.error.clone() {
            ui.label(RichText::new(error).color(theme::red()));
        }

        ui.label(RichText::new("Source folders").strong());
        let mut remove: Option<usize> = None;
        for (index, entry) in self
            .model
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == EntryKind::Src)
        {
            ui.horizontal(|ui| {
                if ui.small_button("×").on_hover_text("Remover").clicked() {
                    remove = Some(index);
                }
                ui.label(RichText::new(&entry.path).monospace().size(12.5));
            });
        }
        if let Some(index) = remove {
            self.model.entries.remove(index);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_src)
                    .hint_text("nova pasta fonte (ex.: src/main/java)")
                    .desired_width(260.0),
            );
            if ui.small_button("Adicionar").clicked() {
                self.add_src();
            }
        });

        ui.add_space(6.0);
        ui.label(RichText::new("Libraries (JARs)").strong());
        let mut remove_lib: Option<usize> = None;
        for (index, entry) in self
            .model
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == EntryKind::Lib)
        {
            ui.horizontal(|ui| {
                if ui.small_button("×").on_hover_text("Remover").clicked() {
                    remove_lib = Some(index);
                }
                ui.label(RichText::new(&entry.path).monospace().size(12.5));
            });
        }
        if let Some(index) = remove_lib {
            self.model.entries.remove(index);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_lib)
                    .hint_text("JAR relativo (ex.: lib/dep.jar)")
                    .desired_width(260.0),
            );
            if ui.small_button("Adicionar").clicked() {
                self.add_lib();
            }
            if ui.small_button("Adicionar JAR…").clicked() {
                self.pick_jar();
            }
        });
        ui.label(
            RichText::new(
                "Dica: use o explorador do SO para copiar o JAR para lib/ e adicione aqui.",
            )
            .small()
            .color(theme::muted()),
        );

        ui.add_space(6.0);
        ui.label(RichText::new("JRE / containers").strong());
        let mut remove_con: Option<usize> = None;
        for (index, entry) in self
            .model
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == EntryKind::Con)
        {
            ui.horizontal(|ui| {
                if ui.small_button("×").on_hover_text("Remover").clicked() {
                    remove_con = Some(index);
                }
                ui.label(RichText::new(&entry.path).monospace().size(12.5));
            });
        }
        if let Some(index) = remove_con {
            self.model.entries.remove(index);
        }
        if !self
            .model
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Con)
            && ui.small_button("Restaurar JRE_CONTAINER padrão").clicked()
        {
            self.model.entries.push(ClasspathEntry {
                kind: EntryKind::Con,
                path: JRE_CONTAINER.to_owned(),
            });
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_container)
                    .hint_text("container (ex.: org.eclipse.../JavaSE-21)")
                    .desired_width(260.0),
            );
            if ui.small_button("Adicionar").clicked() {
                self.add_container();
            }
        });

        ui.add_space(6.0);
        ui.label(RichText::new("Output").strong());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_output)
                    .hint_text("pasta de saída (ex.: bin)")
                    .desired_width(260.0),
            );
            if ui.small_button("Aplicar").clicked() {
                self.apply_output();
            }
        });

        if !self.model.other_lines.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "{} entrada(s) preservada(s) sem edição (ex.: variáveis).",
                    self.model.other_lines.len()
                ))
                .small()
                .color(theme::muted()),
            );
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_classpath_entries() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<classpath>\n\
            \t<classpathentry kind=\"src\" path=\"src/main/java\"/>\n\
            \t<classpathentry kind=\"lib\" path=\"lib/dep.jar\"/>\n\
            \t<classpathentry kind=\"con\" path=\"org.eclipse.jdt.launching.JRE_CONTAINER\"/>\n\
            \t<classpathentry kind=\"output\" path=\"bin\"/>\n\
            \t<classpathentry kind=\"var\" path=\"M2_REPO/x.jar\"/>\n\
            </classpath>\n";
        let model = parse_classpath(text);
        assert_eq!(model.entries.len(), 4);
        assert_eq!(model.other_lines.len(), 1);
        let serialized = serialize_classpath(&model);
        let reparsed = parse_classpath(&serialized);
        assert_eq!(reparsed.entries.len(), 4);
        assert_eq!(reparsed.other_lines.len(), 1);
        assert!(serialized.contains("kind=\"output\" path=\"bin\""));
    }
}
