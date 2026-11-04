//! Configurable keyboard shortcuts.

use std::collections::HashMap;

use egui::{Context, Key, Modifiers};

use crate::session::Keybindings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    QuickOpen,
    OpenType,
    QuickOutline,
    CommandPalette,
    OpenFile,
    OpenFolder,
    Save,
    CloseTab,
    FindInFile,
    ReplaceInFile,
    FindInProject,
    GoToDefinition,
    Reindex,
    FindNext,
    FindPrevious,
    ShortcutHelp,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub modifiers: Modifiers,
    pub key: Key,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: HashMap<KeyAction, Binding>,
}

impl Keymap {
    pub fn from_config(config: &Keybindings) -> Self {
        let defaults = Keybindings::default();
        let mut bindings = HashMap::new();
        let entries = [
            (
                KeyAction::QuickOpen,
                config.quick_open.as_str(),
                defaults.quick_open.as_str(),
            ),
            (
                KeyAction::OpenType,
                config.open_type.as_str(),
                defaults.open_type.as_str(),
            ),
            (
                KeyAction::QuickOutline,
                config.quick_outline.as_str(),
                defaults.quick_outline.as_str(),
            ),
            (
                KeyAction::CommandPalette,
                config.command_palette.as_str(),
                defaults.command_palette.as_str(),
            ),
            (
                KeyAction::OpenFile,
                config.open_file.as_str(),
                defaults.open_file.as_str(),
            ),
            (
                KeyAction::OpenFolder,
                config.open_folder.as_str(),
                defaults.open_folder.as_str(),
            ),
            (
                KeyAction::Save,
                config.save.as_str(),
                defaults.save.as_str(),
            ),
            (
                KeyAction::CloseTab,
                config.close_tab.as_str(),
                defaults.close_tab.as_str(),
            ),
            (
                KeyAction::FindInFile,
                config.find.as_str(),
                defaults.find.as_str(),
            ),
            (
                KeyAction::ReplaceInFile,
                config.replace.as_str(),
                defaults.replace.as_str(),
            ),
            (
                KeyAction::FindInProject,
                config.project_search.as_str(),
                defaults.project_search.as_str(),
            ),
            (
                KeyAction::GoToDefinition,
                config.go_to_definition.as_str(),
                defaults.go_to_definition.as_str(),
            ),
            (
                KeyAction::Reindex,
                config.reindex.as_str(),
                defaults.reindex.as_str(),
            ),
            (
                KeyAction::FindNext,
                config.find_next.as_str(),
                defaults.find_next.as_str(),
            ),
            (
                KeyAction::FindPrevious,
                config.find_previous.as_str(),
                defaults.find_previous.as_str(),
            ),
            (
                KeyAction::ShortcutHelp,
                config.shortcut_help.as_str(),
                defaults.shortcut_help.as_str(),
            ),
        ];

        for (action, configured, fallback) in entries {
            let binding = parse_binding(configured)
                .or_else(|| parse_binding(fallback))
                .unwrap_or_else(|| Binding {
                    modifiers: Modifiers::NONE,
                    key: Key::F12,
                    label: fallback.to_owned(),
                });
            bindings.insert(action, binding);
        }
        Self { bindings }
    }

    pub fn consume(&self, ctx: &Context, action: KeyAction) -> bool {
        let Some(binding) = self.bindings.get(&action) else {
            return false;
        };
        ctx.input_mut(|input| input.consume_key(binding.modifiers, binding.key))
    }

    pub fn label(&self, action: KeyAction) -> String {
        self.bindings
            .get(&action)
            .map(|binding| binding.label.clone())
            .unwrap_or_default()
    }
}

fn parse_binding(text: &str) -> Option<Binding> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let (key_name, modifier_names) = parts.split_last()?;
    let key = Key::from_name(key_name)?;
    let mut modifiers = Modifiers::NONE;
    for modifier in modifier_names {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" | "command" | "super" | "win" => {
                modifiers |= Modifiers::COMMAND;
            }
            "shift" => modifiers |= Modifiers::SHIFT,
            "alt" | "option" => modifiers |= Modifiers::ALT,
            _ => return None,
        }
    }
    Some(Binding {
        modifiers,
        key,
        label: text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ctrl_and_function_bindings() {
        let binding = parse_binding("Ctrl+Shift+P").unwrap();
        assert_eq!(binding.key, Key::P);
        assert!(binding.modifiers.shift);
        assert!(binding.modifiers.command);
        let binding = parse_binding("F12").unwrap();
        assert_eq!(binding.key, Key::F12);
        assert_eq!(binding.modifiers, Modifiers::NONE);
        let binding = parse_binding("Ctrl+3").unwrap();
        assert_eq!(binding.key, Key::Num3);
    }

    #[test]
    fn invalid_binding_falls_back_to_default() {
        let config = Keybindings {
            quick_open: "NotAKey".to_owned(),
            ..Default::default()
        };
        let keymap = Keymap::from_config(&config);
        assert_eq!(keymap.label(KeyAction::QuickOpen), "Ctrl+Shift+R");
    }
}
