// Own test binary: skills live in GREENCODE_HOME and change every native agent's context there.
use greencode_core::{config::Config, skills, tools};
use serde_json::json;

#[tokio::test]
async fn manage_skill_creates_updates_and_deletes() {
    let home = std::env::temp_dir().join(format!("greencode-skill-test-{}", std::process::id()));
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::env::set_var("GREENCODE_HOME", &home);
    let skill = |action: &str, body: &str| json!({"action": action, "name": "ext-skill-test", "description": "test skill", "body": body});
    assert!(tools::ext::run("manage_skill", &skill("update", "x"), &project).await.is_err());
    tools::ext::run("manage_skill", &skill("create", "first"), &project).await.unwrap();
    assert!(tools::ext::run("manage_skill", &skill("create", "again"), &project).await.is_err());
    tools::ext::run("manage_skill", &skill("update", "second"), &project).await.unwrap();
    assert!(skills::list(&project, &Config::default()).iter().any(|s| s.name == "ext-skill-test"));
    assert!(tools::ext::run("manage_skill", &json!({"action": "create", "name": "../evil", "description": "d", "body": "b"}), &project).await.is_err());
    tools::ext::run("manage_skill", &skill("delete", ""), &project).await.unwrap();
    assert!(!skills::list(&project, &Config::default()).iter().any(|s| s.name == "ext-skill-test"));
    let _ = std::fs::remove_dir_all(&home);
}
