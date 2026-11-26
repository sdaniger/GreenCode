//! The shared triage playbook used by the app's `問題を調査` action and `greencode-cli triage`.
/// Where the person describes what went wrong, when they have not already said it.
pub const PLACEHOLDER: &str = "（ここに何が起きたかを書いてください）";

pub const PLAYBOOK: &str = "GreenCode の不具合を調査してください。\n\
- `~/.greencode` の状態（sessions, runtime, ui.json。config.toml の秘密は表示しない）\n\
- このリポジトリのソース（crates/）\n\
症状: {symptom}\n\
原因の候補と確認手順を示してください。修正や GitHub への投稿は、必ず確認してから行ってください。";

/// The playbook with the reported symptom filled in.
pub fn prompt(symptom: &str) -> String {
    let symptom = symptom.trim();
    PLAYBOOK.replace("{symptom}", if symptom.is_empty() { PLACEHOLDER } else { symptom })
}

#[cfg(test)]
mod tests {
    #[test]
    fn symptom_replaces_the_placeholder_without_touching_the_rest() {
        let quiet = super::prompt("  ");
        assert!(quiet.contains(super::PLACEHOLDER));
        assert!(!quiet.contains("{symptom}"));
        let loud = super::prompt("起動しない\n二行目");
        assert!(loud.contains("症状: 起動しない\n二行目"));
        assert!(!loud.contains(super::PLACEHOLDER));
    }
}
