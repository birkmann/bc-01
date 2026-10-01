//! Global shortcut routing. Legacy keys (Space, ←/→, Shift+←/→, q, s, x, v, /)
//! never fire while focus is in an input, select, textarea, button, link or
//! contenteditable (PLAN §10.1: "never while focus is in an input, select or button").

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    TogglePlay,
    Next,
    Previous,
    SeekForward,
    SeekBack,
    TogglePlanner,
    ToggleSimilar,
    CutTransition,
    ToggleDeck,
    FocusSearch,
    Palette,
    Escape,
}

/// Is the focused element one that uses keys itself?
pub fn focus_swallows_keys(tag: &str, content_editable: bool, role: Option<&str>) -> bool {
    if content_editable {
        return true;
    }
    if matches!(tag.to_ascii_uppercase().as_str(), "INPUT" | "TEXTAREA" | "SELECT" | "BUTTON" | "A" | "SUMMARY") {
        return true;
    }
    matches!(role, Some("textbox" | "combobox" | "listbox" | "menu" | "menuitem" | "option" | "slider" | "tab" | "button" | "searchbox" | "spinbutton"))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub meta: bool,
    pub alt: bool,
}

/// Map a key press to a shortcut. `typing_or_widget` = [`focus_swallows_keys`] of the target.
pub fn map_key(key: &str, m: Mods, typing_or_widget: bool) -> Option<Shortcut> {
    // The palette works everywhere, even from an input.
    if (m.ctrl || m.meta) && !m.alt && key.eq_ignore_ascii_case("k") {
        return Some(Shortcut::Palette);
    }
    if key == "Escape" {
        return Some(Shortcut::Escape);
    }
    if typing_or_widget || m.ctrl || m.meta || m.alt {
        return None;
    }
    match key {
        " " => Some(Shortcut::TogglePlay),
        "ArrowRight" if m.shift => Some(Shortcut::Next),
        "ArrowLeft" if m.shift => Some(Shortcut::Previous),
        "ArrowRight" => Some(Shortcut::SeekForward),
        "ArrowLeft" => Some(Shortcut::SeekBack),
        "q" | "Q" => Some(Shortcut::TogglePlanner),
        "s" | "S" => Some(Shortcut::ToggleSimilar),
        "x" | "X" => Some(Shortcut::CutTransition),
        "v" | "V" => Some(Shortcut::ToggleDeck),
        "/" => Some(Shortcut::FocusSearch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NONE: Mods = Mods { shift: false, ctrl: false, meta: false, alt: false };

    #[test]
    fn legacy_keys_map() {
        assert_eq!(map_key(" ", NONE, false), Some(Shortcut::TogglePlay));
        assert_eq!(map_key("ArrowRight", Mods { shift: true, ..NONE }, false), Some(Shortcut::Next));
        assert_eq!(map_key("ArrowLeft", Mods { shift: true, ..NONE }, false), Some(Shortcut::Previous));
        assert_eq!(map_key("ArrowRight", NONE, false), Some(Shortcut::SeekForward));
        assert_eq!(map_key("q", NONE, false), Some(Shortcut::TogglePlanner));
        assert_eq!(map_key("s", NONE, false), Some(Shortcut::ToggleSimilar));
        assert_eq!(map_key("x", NONE, false), Some(Shortcut::CutTransition));
        assert_eq!(map_key("v", NONE, false), Some(Shortcut::ToggleDeck));
        assert_eq!(map_key("/", NONE, false), Some(Shortcut::FocusSearch));
    }

    #[test]
    fn never_fire_in_inputs_selects_or_buttons() {
        for tag in ["input", "TEXTAREA", "select", "button"] {
            assert!(focus_swallows_keys(tag, false, None));
            assert_eq!(map_key(" ", NONE, focus_swallows_keys(tag, false, None)), None);
        }
        assert!(focus_swallows_keys("div", true, None));
        assert!(focus_swallows_keys("div", false, Some("combobox")));
        assert!(!focus_swallows_keys("div", false, None));
        assert!(!focus_swallows_keys("body", false, None));
    }

    #[test]
    fn modifiers_disable_plain_keys_but_palette_works_everywhere() {
        assert_eq!(map_key("q", Mods { ctrl: true, ..NONE }, false), None);
        assert_eq!(map_key("k", Mods { ctrl: true, ..NONE }, true), Some(Shortcut::Palette));
        assert_eq!(map_key("K", Mods { meta: true, ..NONE }, false), Some(Shortcut::Palette));
    }
}
