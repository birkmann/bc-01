//! The live set plan as server-side state: a port of the browser's `planStore`
//! actions as pure functions over [`PlanState`], so every client (desktop,
//! phone) edits one plan.

use bc_types::player::{PlanOp, PlanState, TagRulePreset, TagRules};

fn norm(t: &str) -> String {
    t.trim().to_lowercase()
}

fn toggle_in(list: &[String], tag: &str) -> Vec<String> {
    let key = norm(tag);
    if list.iter().any(|t| norm(t) == key) {
        list.iter().filter(|t| norm(t) != key).cloned().collect()
    } else {
        let mut v = list.to_vec();
        v.push(tag.to_string());
        v
    }
}

fn without(list: &[String], tag: &str) -> Vec<String> {
    list.iter().filter(|t| norm(t) != norm(tag)).cloned().collect()
}

/// New live rules, keeping the active preset only while the rules still match
/// it, so a preset chip lights up exactly when its rules are in force.
fn with_rules(s: &mut PlanState, rules: TagRules) {
    let keep = s
        .active_preset_id
        .as_ref()
        .and_then(|id| s.tag_presets.iter().find(|p| &p.id == id))
        .map(|p| p.rules.same_as(&rules))
        .unwrap_or(false);
    if !keep {
        s.active_preset_id = None;
    }
    s.tag_rules = rules;
}

fn preset_id(n: usize) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("p{t:x}{n:x}")
}

/// Apply one planner operation. Returns whether anything changed.
pub fn apply(s: &mut PlanState, op: PlanOp, now_ms: u64) -> bool {
    let before = s.clone();
    match op {
        PlanOp::SetTempo { tempo } => s.tempo = tempo,
        PlanOp::SetEnergy { energy } => s.energy = energy,
        PlanOp::SetTagMode { mode } => s.tag_mode = mode,
        PlanOp::SetTargetTags { tags } => s.target_tags = tags,
        PlanOp::ToggleTargetTag { tag } => s.target_tags = toggle_in(&s.target_tags, &tag),
        PlanOp::SetHarmonic { harmonic } => s.harmonic = harmonic,
        PlanOp::ToggleAllowTag { tag } => {
            let r = TagRules { allow: toggle_in(&s.tag_rules.allow, &tag), deny: without(&s.tag_rules.deny, &tag) };
            with_rules(s, r);
        }
        PlanOp::ToggleDenyTag { tag } => {
            let r = TagRules { allow: without(&s.tag_rules.allow, &tag), deny: toggle_in(&s.tag_rules.deny, &tag) };
            with_rules(s, r);
        }
        PlanOp::SetTagRules { rules } => with_rules(s, rules),
        PlanOp::ClearTagRules => {
            s.tag_rules = TagRules::default();
            s.active_preset_id = None;
        }
        PlanOp::SavePreset { name } => {
            let label = name.trim().to_string();
            if label.is_empty() {
                return false;
            }
            let id = preset_id(s.tag_presets.len());
            s.tag_presets.push(TagRulePreset { id: id.clone(), name: label, rules: s.tag_rules.clone() });
            s.active_preset_id = Some(id);
        }
        PlanOp::UpdatePreset { id } => {
            let rules = s.tag_rules.clone();
            if let Some(p) = s.tag_presets.iter_mut().find(|p| p.id == id) {
                p.rules = rules;
                s.active_preset_id = Some(id);
            }
        }
        PlanOp::ApplyPreset { id } => {
            if let Some(p) = s.tag_presets.iter().find(|p| p.id == id).cloned() {
                // applying the active preset again reloads its rules ("the rules, not the edits")
                s.tag_rules = p.rules;
                s.active_preset_id = Some(id);
            }
        }
        PlanOp::RenamePreset { id, name } => {
            let label = name.trim().to_string();
            if label.is_empty() {
                return false;
            }
            if let Some(p) = s.tag_presets.iter_mut().find(|p| p.id == id) {
                p.name = label;
            }
        }
        PlanOp::DeletePreset { id } => {
            s.tag_presets.retain(|p| p.id != id);
            if s.active_preset_id.as_deref() == Some(id.as_str()) {
                // the live rules stay: deleting a preset forgets the name, not the set
                s.active_preset_id = None;
            }
        }
        PlanOp::SetAutoFill { on } => s.auto_fill = on,
        PlanOp::MixFrom { pool } => {
            let key = pool.key();
            let mut pools = vec![pool];
            pools.extend(s.pools.iter().filter(|p| p.key() != key).cloned());
            s.pools = pools;
        }
        PlanOp::ChainPool { pool } => {
            if !s.pools.iter().any(|p| p.key() == pool.key()) {
                s.pools.push(pool);
            }
        }
        PlanOp::RemovePool { key } => s.pools.retain(|p| p.key() != key),
        PlanOp::AdvancePool => {
            if !s.pools.is_empty() {
                s.pools.remove(0);
            }
        }
        PlanOp::SetPools { pools } => s.pools = pools,
        PlanOp::AddWish { wish } => {
            let key = wish.key();
            if !s.wishes.iter().any(|w| w.key() == key) {
                s.wishes.push(wish);
            }
        }
        PlanOp::RemoveWish { key } => s.wishes.retain(|w| w.key() != key),
        PlanOp::ClearWishes => s.wishes.clear(),
        PlanOp::SetSetLength { minutes } => s.set_length_min = minutes,
        PlanOp::StartSet => s.set_started_at_ms = Some(now_ms),
        PlanOp::StopSet => s.set_started_at_ms = None,
        PlanOp::Reset => {
            // saved presets are a library, not a direction: they survive a reset
            let presets = std::mem::take(&mut s.tag_presets);
            *s = PlanState { tag_presets: presets, ..PlanState::default() };
        }
    }
    *s != before
}

/// Time left on the set clock, or `None` when no set is running.
pub fn remaining_ms(started_ms: Option<u64>, length_min: Option<f64>, now_ms: u64) -> Option<f64> {
    let (s, l) = (started_ms?, length_min?);
    Some(s as f64 + l * 60_000.0 - now_ms as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::player::{Pool, Wish};

    #[test]
    fn allow_lifts_deny_and_back() {
        let mut s = PlanState::default();
        apply(&mut s, PlanOp::ToggleDenyTag { tag: "Ambient".into() }, 0);
        assert_eq!(s.tag_rules.deny, vec!["Ambient"]);
        apply(&mut s, PlanOp::ToggleAllowTag { tag: "ambient".into() }, 0);
        assert!(s.tag_rules.deny.is_empty());
        assert_eq!(s.tag_rules.allow, vec!["ambient"]);
        apply(&mut s, PlanOp::ToggleAllowTag { tag: "AMBIENT".into() }, 0);
        assert!(s.tag_rules.allow.is_empty());
    }

    #[test]
    fn preset_lights_up_only_while_rules_match() {
        let mut s = PlanState::default();
        apply(&mut s, PlanOp::ToggleAllowTag { tag: "techno".into() }, 0);
        apply(&mut s, PlanOp::SavePreset { name: "Warm".into() }, 0);
        assert!(s.active_preset_id.is_some());
        apply(&mut s, PlanOp::ToggleDenyTag { tag: "pop".into() }, 0);
        assert!(s.active_preset_id.is_none());
        let id = s.tag_presets[0].id.clone();
        apply(&mut s, PlanOp::ApplyPreset { id: id.clone() }, 0);
        assert_eq!(s.active_preset_id.as_deref(), Some(id.as_str()));
        assert!(s.tag_rules.deny.is_empty());
        apply(&mut s, PlanOp::DeletePreset { id }, 0);
        assert!(s.active_preset_id.is_none());
        assert_eq!(s.tag_rules.allow, vec!["techno"]); // the set stays
    }

    #[test]
    fn pools_chain_and_advance() {
        let mut s = PlanState::default();
        let pl = Pool::Playlist { id: 3, name: "Warm".into() };
        apply(&mut s, PlanOp::MixFrom { pool: Pool::Loved }, 0);
        apply(&mut s, PlanOp::ChainPool { pool: Pool::Library }, 0);
        apply(&mut s, PlanOp::ChainPool { pool: Pool::Library }, 0);
        assert_eq!(s.pools.len(), 2);
        apply(&mut s, PlanOp::MixFrom { pool: pl.clone() }, 0);
        assert_eq!(s.pools[0], pl);
        assert_eq!(s.pools.len(), 3);
        apply(&mut s, PlanOp::MixFrom { pool: Pool::Library }, 0);
        assert_eq!(s.pools[0], Pool::Library);
        assert_eq!(s.pools.len(), 3);
        apply(&mut s, PlanOp::AdvancePool, 0);
        assert_eq!(s.pools.len(), 2);
    }

    #[test]
    fn wishes_dedupe_and_reset_keeps_presets() {
        let mut s = PlanState::default();
        let w = Wish::Tag { name: "Dub".into() };
        apply(&mut s, PlanOp::AddWish { wish: w.clone() }, 0);
        assert!(!apply(&mut s, PlanOp::AddWish { wish: Wish::Tag { name: "dub".into() } }, 0));
        assert_eq!(s.wishes.len(), 1);
        apply(&mut s, PlanOp::SavePreset { name: "x".into() }, 0);
        apply(&mut s, PlanOp::Reset, 0);
        assert!(s.wishes.is_empty());
        assert_eq!(s.tag_presets.len(), 1);
    }

    #[test]
    fn set_clock() {
        let mut s = PlanState::default();
        apply(&mut s, PlanOp::SetSetLength { minutes: Some(60.0) }, 0);
        apply(&mut s, PlanOp::StartSet, 1_000);
        assert_eq!(remaining_ms(s.set_started_at_ms, s.set_length_min, 61_000), Some(3_540_000.0));
        assert_eq!(remaining_ms(None, Some(1.0), 0), None);
    }
}
