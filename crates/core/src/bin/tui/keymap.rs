use super::input::Edit;
use anyhow::{bail, ensure, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as Mods};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action { Model, NextModel, PrevModel, Plan, Thinking, History, FollowUp, Dequeue, ExpandTools, ToggleTools, ToggleThinking, ExternalEditor, Edit(Edit) }

const ACTIONS: &[(Action, &str, &str, &[&str])] = &[
    (Action::Model, "app.model.select", "Model picker", &["Alt+M", "Ctrl+L"]),
    (Action::NextModel, "app.model.cycleForward", "Next model", &["Ctrl+P"]),
    (Action::PrevModel, "app.model.cycleBackward", "Previous model", &["Ctrl+Shift+P"]),
    (Action::Plan, "app.plan.toggle", "Toggle plan", &["Alt+Shift+P"]),
    (Action::Thinking, "app.thinking.cycle", "Cycle thinking", &["Shift+Tab"]),
    (Action::History, "app.history.search", "Search prompt history", &["Ctrl+R"]),
    (Action::FollowUp, "app.message.followUp", "Queue a follow-up", &["Ctrl+Q", "Ctrl+Enter"]),
    (Action::Dequeue, "app.message.dequeue", "Recall last queued prompt", &["Alt+Up", "Shift+Up"]),
    (Action::ExpandTools, "app.tools.expand", "Expand tool output", &["Ctrl+O"]),
    (Action::ToggleTools, "app.tools.toggleVisibility", "Show or hide tool activity", &["Ctrl+Shift+O"]),
    (Action::ToggleThinking, "app.thinking.toggle", "Show or hide thinking", &["Ctrl+T"]),
    (Action::ExternalEditor, "app.editor.external", "Edit draft in external editor", &["Ctrl+G"]),
    (Action::Edit(Edit::Up), "tui.editor.cursorUp", "Move cursor up / older prompt", &["Up"]),
    (Action::Edit(Edit::Down), "tui.editor.cursorDown", "Move cursor down / newer prompt", &["Down"]),
    (Action::Edit(Edit::Left), "tui.editor.cursorLeft", "Move cursor left", &["Left", "Ctrl+B"]),
    (Action::Edit(Edit::Right), "tui.editor.cursorRight", "Move cursor right", &["Right", "Ctrl+F"]),
    (Action::Edit(Edit::WordLeft), "tui.editor.cursorWordLeft", "Move word left", &["Alt+Left", "Ctrl+Left", "Alt+B"]),
    (Action::Edit(Edit::WordRight), "tui.editor.cursorWordRight", "Move word right", &["Alt+Right", "Ctrl+Right", "Alt+F"]),
    (Action::Edit(Edit::LineStart), "tui.editor.cursorLineStart", "Move to line start", &["Home", "Ctrl+A"]),
    (Action::Edit(Edit::LineEnd), "tui.editor.cursorLineEnd", "Move to line end", &["End", "Ctrl+E"]),
    (Action::Edit(Edit::Backspace), "tui.editor.deleteCharBackward", "Delete previous character", &["Backspace"]),
    (Action::Edit(Edit::Delete), "tui.editor.deleteCharForward", "Delete next character", &["Delete"]),
    (Action::Edit(Edit::DeleteWordLeft), "tui.editor.deleteWordBackward", "Delete previous word", &["Ctrl+W", "Alt+Backspace", "Ctrl+Backspace", "Super+Alt+Backspace"]),
    (Action::Edit(Edit::DeleteWordRight), "tui.editor.deleteWordForward", "Delete next word", &["Alt+Delete", "Alt+D", "Ctrl+Delete", "Super+Alt+Delete", "Super+Alt+D"]),
    (Action::Edit(Edit::DeleteToStart), "tui.editor.deleteToLineStart", "Delete to line start", &["Ctrl+U"]),
    (Action::Edit(Edit::DeleteToEnd), "tui.editor.deleteToLineEnd", "Delete to line end", &["Ctrl+K"]),
];

type Chord = (KeyCode, Mods);

#[derive(Deserialize)]
#[serde(untagged)]
enum Keys { One(String), Many(Vec<String>) }

pub(super) struct Keymap(Vec<(Chord, Action, String)>);

impl Keymap {
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut overrides = BTreeMap::new();
        for path in paths {
            let text = std::fs::read_to_string(path).with_context(|| format!("Cannot read keymap {}", path.display()))?;
            let layer: BTreeMap<String, Keys> = serde_saphyr::from_str(&text)
                .with_context(|| format!("Invalid keymap {}", path.display()))?;
            overrides.extend(layer);
        }
        Self::compile(&overrides)
    }

    fn compile(overrides: &BTreeMap<String, Keys>) -> Result<Self> {
        let mut bindings = Vec::<(Chord, Action, String)>::new();
        for (action, id, _, _) in ACTIONS {
            let keys: &[String] = match overrides.get(*id) {
                Some(Keys::One(key)) => std::slice::from_ref(key),
                Some(Keys::Many(keys)) => keys,
                None => continue,
            };
            for key in keys {
                let chord = parse(key, *action).with_context(|| format!("{id}: {key:?}"))?;
                if let Some((_, other, _)) = bindings.iter().find(|(bound, ..)| *bound == chord) {
                    ensure!(action == other, "Key {key:?} is assigned to multiple actions");
                } else {
                    bindings.push((chord, *action, key.clone()));
                }
            }
        }
        for (action, id, _, defaults) in ACTIONS {
            if overrides.contains_key(*id) { continue; }
            for key in *defaults {
                let chord = parse(key, *action)?;
                if !bindings.iter().any(|(bound, ..)| *bound == chord) {
                    bindings.push((chord, *action, (*key).into()));
                }
            }
        }
        Ok(Self(bindings))
    }

    pub fn action(&self, key: KeyEvent) -> Option<Action> {
        let chord = match key.code {
            KeyCode::Char(c) => (KeyCode::Char(c.to_ascii_lowercase()), key.modifiers),
            KeyCode::BackTab => (KeyCode::Tab, key.modifiers | Mods::SHIFT),
            code => (code, key.modifiers),
        };
        self.0.iter().find(|(bound, ..)| *bound == chord).map(|(_, action, _)| *action)
    }

    pub fn help(&self) -> String {
        ACTIONS.iter().map(|(action, _, label, _)| {
            let keys = self.0.iter().filter(|(_, a, _)| a == action).map(|(_, _, k)| k.as_str()).collect::<Vec<_>>();
            format!("{label}: {}", if keys.is_empty() { "(unbound)".into() } else { keys.join(" / ") })
        }).collect::<Vec<_>>().join("\n")
    }
}

fn parse(key: &str, action: Action) -> Result<Chord> {
    let key = key.to_ascii_lowercase();
    let mut parts = key.split('+').map(str::trim).peekable();
    let mut mods = Mods::empty();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            let modifier = match part {
                "ctrl" => Mods::CONTROL, "alt" => Mods::ALT, "shift" => Mods::SHIFT, "cmd" | "super" => Mods::SUPER,
                _ => bail!("Unknown modifier {part:?}"),
            };
            ensure!(!mods.contains(modifier), "Duplicate modifier {part:?}");
            mods |= modifier;
            continue;
        }
        let code = match part {
            "up" => KeyCode::Up, "down" => KeyCode::Down, "left" => KeyCode::Left, "right" => KeyCode::Right,
            "enter" => KeyCode::Enter, "tab" => KeyCode::Tab, "backspace" => KeyCode::Backspace,
            "pageup" => KeyCode::PageUp, "pagedown" => KeyCode::PageDown, "home" => KeyCode::Home, "end" => KeyCode::End,
            "delete" => KeyCode::Delete, "insert" => KeyCode::Insert, "space" => KeyCode::Char(' '),
            _ if part.len() == 1 && part.as_bytes()[0].is_ascii_graphic() => KeyCode::Char(part.as_bytes()[0] as char),
            _ if part.starts_with('f') => KeyCode::F(part[1..].parse::<u8>().ok().filter(|n| (1..=24).contains(n)).context("Expected F1–F24")?),
            _ => bail!("Unsupported key {part:?}"),
        };
        if matches!(action, Action::Edit(_)) {
            ensure!(!matches!(code, KeyCode::Enter | KeyCode::PageUp | KeyCode::PageDown)
                && !(code == KeyCode::Tab && mods.is_empty()), "The composer reserves this key");
        } else {
            ensure!(matches!(code, KeyCode::F(_)) || !mods.is_empty(), "An application binding needs a modifier or function key");
        }
        if let KeyCode::Char(c) = code {
            ensure!(mods.intersects(Mods::ALT | Mods::CONTROL | Mods::SUPER), "A printable binding needs Ctrl, Alt, or Cmd");
            ensure!(!(matches!(c, 'c' | 'd') && mods.contains(Mods::CONTROL)), "Ctrl+C and Ctrl+D are reserved");
            ensure!(!matches!(action, Action::Edit(_)) || !matches!(c, 'j' | 's')
                || !mods.contains(Mods::CONTROL), "The composer reserves this key");
        }
        return Ok((code, mods));
    }
    bail!("Missing key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_yaml_remap_wins_and_empty_array_unbinds() {
        let map = Keymap::compile(&serde_saphyr::from_str("app.model.select: [Ctrl+R, Alt+B]\napp.plan.toggle: []\n").unwrap()).unwrap();
        let event = |c, mods| KeyEvent::new(KeyCode::Char(c), mods);
        assert_eq!(map.action(event('r', Mods::CONTROL)), Some(Action::Model));
        assert_eq!(map.action(event('m', Mods::ALT)), None);
        assert_eq!(map.action(event('P', Mods::ALT | Mods::SHIFT)), None);
        assert_eq!(map.action(KeyEvent::new(KeyCode::BackTab, Mods::empty())), Some(Action::Thinking));
        assert!(map.help().contains("Model picker: Ctrl+R / Alt+B"));
        assert!(Keymap::compile(&serde_saphyr::from_str("app.model.select: x\n").unwrap()).is_err());
    }

    #[test]
    fn remapped_editor_keys_replace_defaults_and_respect_graphemes() {
        let map = Keymap::compile(&serde_saphyr::from_str(
            "tui.editor.cursorWordLeft: F6\ntui.editor.deleteCharBackward: []\ntui.editor.deleteWordForward: Super+Alt+D\n"
        ).unwrap()).unwrap();
        let mut editor = super::super::input::Editor::default();
        editor.replace("日👩‍💻 café".into());
        for key in [KeyEvent::from(KeyCode::Backspace), KeyEvent::from(KeyCode::F(6)),
            KeyEvent::new(KeyCode::Char('d'), Mods::SUPER | Mods::ALT)] {
            let edit = match map.action(key) { Some(Action::Edit(edit)) => Some(edit), _ => None };
            editor.key(key, edit);
        }
        assert_eq!(editor.text(), "日👩‍💻 ");
        assert_eq!(map.action(KeyEvent::new(KeyCode::Left, Mods::ALT)), None);
    }
}
