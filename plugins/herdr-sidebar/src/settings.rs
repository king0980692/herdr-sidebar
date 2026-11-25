//! The sidebar's settings as a host shows them in its own settings page:
//! a list of rows (a toggle, a choice, or a line of text) read from and
//! written to the sidebar's state, under its state lock. The docked sidebar
//! picks up a change on its next heartbeat.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Toggle,
    Choice { options: Vec<String> },
    Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub kind: Kind,
    pub value: Value,
    /// Disabled rows are shown but cannot change.
    pub enabled: bool,
}

fn toggle(key: &str, label: &str, help: &str, value: bool) -> Setting {
    Setting {
        key: key.into(),
        label: label.into(),
        help: Some(help.into()),
        kind: Kind::Toggle,
        value: Value::Bool(value),
        enabled: true,
    }
}

fn choice(key: &str, label: &str, help: &str, options: &[&str], value: &str) -> Setting {
    Setting {
        key: key.into(),
        label: label.into(),
        help: Some(help.into()),
        kind: Kind::Choice {
            options: options.iter().map(|option| (*option).to_owned()).collect(),
        },
        value: Value::String(value.into()),
        enabled: true,
    }
}

/// The settings a docked sidebar uses, with their current values.
pub fn list() -> Vec<Setting> {
    let state = crate::state::load_state();
    let icons = match state.icons {
        Some(crate::icons::IconTheme::Emoji) => "emoji",
        Some(crate::icons::IconTheme::Material) => "material",
        None => "auto",
    };
    let editor = crate::actions::configured_editor();
    vec![
        choice(
            "icons",
            "Icon theme",
            "material needs a Nerd Font; auto picks by probing for one",
            &["auto", "material", "emoji"],
            icons,
        ),
        choice(
            "color_theme",
            "Color theme",
            "terminal follows your terminal's palette",
            &["vscode", "light", "terminal"],
            state.color_theme.label(),
        ),
        choice(
            "preview_placement",
            "Preview opens in",
            "its own tab, or a pane beside the viewed one",
            &["tab", "pane"],
            match state.preview_placement {
                crate::state::PreviewPlacement::Tab => "tab",
                _ => "pane",
            },
        ),
        Setting {
            key: "custom_editor".into(),
            label: "Custom editor".into(),
            help: Some("the command files open with, e.g. nvim".into()),
            kind: Kind::Text,
            value: Value::String(crate::state::load_editor_command().unwrap_or_default()),
            enabled: true,
        },
        Setting {
            enabled: editor.is_some(),
            ..toggle(
                "custom_editor_on_click",
                "Open files in the editor on click",
                "Enter still previews",
                state.custom_editor_on_click,
            )
        },
        toggle(
            "follow_cwd",
            "Follow the pane's folder",
            "the tree moves when the viewed pane changes directory",
            state.follow_cwd,
        ),
        toggle(
            "git_deco",
            "Git decorations",
            "status letters in the tree; off stops git polling",
            state.git_deco,
        ),
        toggle(
            "show_git_footer",
            "Git footer",
            "branch and sync strip at the bottom",
            state.show_git_footer,
        ),
        toggle(
            "show_hotkeys",
            "Footer hotkeys",
            "key hints at the bottom",
            state.show_hotkeys,
        ),
        toggle(
            "scm_tree_view",
            "Source Control as a tree",
            "off lists changed files flat",
            state.scm_tree_view,
        ),
    ]
}

/// Sets `key` to `value` (a bool for a toggle, an option's name for a
/// choice, a string for text).
pub fn set(key: &str, value: &Value) -> Result<(), String> {
    let as_bool = || {
        value
            .as_bool()
            .ok_or_else(|| format!("{key} takes true or false"))
    };
    let as_str = || {
        value
            .as_str()
            .ok_or_else(|| format!("{key} takes a string"))
    };
    match key {
        "custom_editor" => {
            let command = as_str()?.trim();
            if command.is_empty() {
                return crate::state::clear_editor_command()
                    .then_some(())
                    .ok_or_else(|| "the editor command could not be cleared".into());
            }
            return crate::state::save_editor_command(command)
                .then_some(())
                .ok_or_else(|| "the editor command must be one line".into());
        }
        "icons" => {
            let icons = match as_str()? {
                "auto" => None,
                "material" => Some(crate::icons::IconTheme::Material),
                "emoji" => Some(crate::icons::IconTheme::Emoji),
                other => return Err(format!("no icon theme {other}")),
            };
            crate::state::update_state(|state| state.icons = icons);
        }
        "color_theme" => {
            let theme = crate::state::ColorTheme::from_label(as_str()?)
                .ok_or_else(|| "no such color theme".to_owned())?;
            crate::state::update_state(|state| state.color_theme = theme);
        }
        "preview_placement" => {
            let placement = match as_str()? {
                "tab" => crate::state::PreviewPlacement::Tab,
                "pane" => crate::state::PreviewPlacement::Pane,
                other => return Err(format!("no preview placement {other}")),
            };
            crate::state::update_state(|state| state.preview_placement = placement);
        }
        "custom_editor_on_click" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.custom_editor_on_click = on);
        }
        "follow_cwd" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.follow_cwd = on);
        }
        "git_deco" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.git_deco = on);
        }
        "show_git_footer" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.show_git_footer = on);
        }
        "show_hotkeys" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.show_hotkeys = on);
        }
        "scm_tree_view" => {
            let on = as_bool()?;
            crate::state::update_state(|state| state.scm_tree_view = on);
        }
        other => return Err(format!("no setting {other}")),
    }
    Ok(())
}

/// When the settings were last changed (by anyone), to reload on change.
pub fn changed_at() -> Option<std::time::SystemTime> {
    [
        crate::state::settings_path(),
        crate::state::state_path(),
        crate::state::editor_command_path(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    })
    .max()
}
