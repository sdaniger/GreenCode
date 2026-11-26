//! `goal.enabled`: a per-session goal plus the hidden `goal` tool. An active goal with remaining budget
//! keeps the agent working between turns; the goal line and its budget live in `session::runtime`.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Goal {
    pub objective: String,
    pub status: Status,
    pub tokens_used: u64,
    pub token_budget: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Active,
    Paused,
    Complete,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self { Status::Active => "active", Status::Paused => "paused", Status::Complete => "complete" }
    }
    fn parse(value: &str) -> Option<Status> {
        match value { "active" => Some(Status::Active), "paused" => Some(Status::Paused), "complete" => Some(Status::Complete), _ => None }
    }
}

impl Goal {
    pub fn over_budget(&self) -> bool {
        self.token_budget.is_some_and(|budget| self.tokens_used >= budget)
    }

    /// The continuation prompt while the goal stays open; `None` once it is complete, paused or over budget.
    pub fn continuation(&self) -> Option<String> {
        (self.status == Status::Active && !self.over_budget()).then(|| {
            format!("Continue working toward the goal: {}\nWhen it is completely done and verified, call the `goal` tool with status `complete`.", self.objective)
        })
    }

    /// The continuation prompt when `goal.continuation_modes` allows this run mode; the config
    /// decides which of interactive, rpc/headless continues automatically.
    pub fn continues(&self, modes: &crate::config::modes::ModesOpts, run: crate::config::modes::RunMode) -> Option<String> {
        modes.continues(run).then(|| self.continuation()).flatten()
    }

    /// The footer line: the objective and, with `footer`, its budget.
    pub fn line(&self, footer: bool) -> String {
        let status = match self.status { Status::Active => "", Status::Paused => " (paused)", Status::Complete => " (complete)" };
        let budget = match (footer, self.token_budget) {
            (true, Some(budget)) => format!(" · {}/{} tokens", self.tokens_used, budget),
            _ => String::new(),
        };
        format!("goal: {}{status}{budget}", self.objective)
    }

    /// Reads the session's goal; a missing or broken record means no goal.
    pub fn load(session: &str) -> Option<Goal> {
        let value = crate::session::Session::runtime(session).ok()?;
        Self::from_value(&value["goal"])
    }

    pub fn from_value(value: &Value) -> Option<Goal> {
        Some(Goal {
            objective: value["objective"].as_str().filter(|s| !s.is_empty())?.to_string(),
            status: value["status"].as_str().and_then(Status::parse).unwrap_or_default(),
            tokens_used: value["tokens_used"].as_u64().unwrap_or(0),
            token_budget: value["token_budget"].as_u64(),
        })
    }

    pub fn to_value(&self) -> Value {
        json!({"objective": self.objective, "status": self.status.as_str(), "tokens_used": self.tokens_used, "token_budget": self.token_budget})
    }

    /// Writes this goal into the session's runtime record, keeping its other keys.
    pub fn save(&self, session: &str) -> Result<()> {
        let mut value = crate::session::Session::runtime(session).unwrap_or(Value::Null);
        if !value.is_object() { value = json!({}); }
        value["goal"] = self.to_value();
        crate::session::Session::save_runtime(session, &value)
    }

    pub fn clear(session: &str) -> Result<()> {
        let mut value = crate::session::Session::runtime(session).unwrap_or(Value::Null);
        if value.get("goal").is_none() { return Ok(()); }
        if let Some(object) = value.as_object_mut() { object.remove("goal"); }
        crate::session::Session::save_runtime(session, &value)
    }
}

/// The hidden `goal` tool definition; only present while goal mode is on.
pub fn tool_def() -> crate::ToolDef {
    crate::ToolDef {
        name: "goal".into(),
        description: "Set, pause, resume or finish this session's goal. `create` needs an objective; `budget` is a token budget for it. Progress continues between turns until complete or paused.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {"enum": ["create", "pause", "resume", "complete", "status", "drop"]},
                "objective": {"type": "string"},
                "budget": {"type": "integer", "minimum": 0}
            },
            "required": ["action"]
        }),
    }
}

/// Applies one `goal` call to disk and returns what to tell the model.
pub fn apply(session: Option<&str>, input: &Value) -> Result<String> {
    let action = input["action"].as_str().unwrap_or("");
    let session = session.ok_or_else(|| anyhow::anyhow!("goal mode needs a saved session"))?;
    let mut goal = Goal::load(session);
    match action {
        "create" => {
            let objective = input["objective"].as_str().map(str::trim).filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`goal` create needs a non-empty `objective`"))?;
            if goal.as_ref().is_some_and(|g| g.status == Status::Active) {
                bail!("a goal is already active: `pause` or `drop` it before setting a new objective");
            }
            let token_budget = match input["budget"].as_u64() {
                Some(0) | None => None,
                Some(budget) => Some(budget),
            };
            Goal { objective: objective.into(), status: Status::Active, tokens_used: 0, token_budget }.save(session)?;
            Ok("goal created; it continues between turns until complete or paused".into())
        }
        "pause" | "resume" | "complete" | "drop" | "status" => {
            let Some(mut goal) = goal.take() else { bail!("no goal is set") };
            match action {
                "pause" => goal.status = Status::Paused,
                "resume" => goal.status = Status::Active,
                "complete" => goal.status = Status::Complete,
                "status" => {}
                "drop" => {
                    Goal::clear(session)?;
                    return Ok("goal dropped".into());
                }
                _ => {}
            }
            goal.save(session)?;
            Ok(goal.line(true))
        }
        other => bail!("unknown goal action `{other}`; use create, pause, resume, complete, status or drop"),
    }
}

/// The folder-level message inbox other GreenCode instances use (`task.crossInstance`).
pub fn inbox(dir: &std::path::Path) -> PathBuf { dir.join(".greencode/agents.jsonl") }

/// Adds this run's usage to the session goal, if one is open.
pub fn count_tokens(session: &Option<String>, tokens: u64) {
    let Some(session) = session.as_deref() else { return };
    let Some(mut goal) = Goal::load(session) else { return };
    goal.tokens_used = goal.tokens_used.saturating_add(tokens);
    let _ = goal.save(session);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_and_continuation() {
        let cfg = crate::config::modes::GoalCfg::default();
        let mut goal = Goal { objective: "ship it".into(), status: Status::Active, tokens_used: 3, token_budget: Some(10) };
        assert!(goal.continuation().unwrap().contains("ship it"));
        assert_eq!(goal.line(true), "goal: ship it · 3/10 tokens");
        assert_eq!(goal.line(false), "goal: ship it");
        goal.tokens_used = 10;
        assert!(goal.over_budget() && goal.continuation().is_none(), "an exhausted budget stops continuations");
        goal.tokens_used = 0;
        goal.status = Status::Paused;
        assert!(goal.continuation().is_none());
        goal.status = Status::Complete;
        assert_eq!(goal.line(false), "goal: ship it (complete)");
        assert!(crate::config::modes::GoalCfg { enabled: false, ..cfg }.continuation_modes.len() == 1, "interactive is the default run mode");
        use crate::config::modes::{ModesOpts, RunMode};
        let mut modes = ModesOpts::default();
        let active = Goal { objective: "ship it".into(), status: Status::Active, tokens_used: 0, token_budget: None };
        assert!(active.continues(&modes, RunMode::Interactive).is_some(), "interactive continues by default");
        assert!(active.continues(&modes, RunMode::Rpc).is_none(), "rpc does not unless configured");
        modes.goal.continuation_modes = vec![RunMode::Rpc];
        assert!(active.continues(&modes, RunMode::Rpc).is_some());
        modes.goal.enabled = false;
        assert!(active.continues(&modes, RunMode::Rpc).is_none(), "goal.enabled off stops continuations");
    }
}
