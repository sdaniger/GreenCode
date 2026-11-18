use super::*;
use greencode_core::session::{AutoSettle, LinkedPr};

struct Facts<'a> {
    idle: bool,
    preference: AutoSettle,
    last_activity: u64,
    last_user_at: u64,
    pr: Option<&'a LinkedPr>,
}
#[derive(Clone, Copy)]
struct Rules {
    inactive: bool,
    merged: bool,
    closed: bool,
    keep_pr: bool,
}

fn should_settle(f: Facts<'_>, r: Rules, now: u64) -> bool {
    if !f.idle || f.preference == AutoSettle::Never {
        return false;
    }
    if let Some(pr) = f.pr {
        if pr.changed_at > f.last_user_at
            && ((pr.state == "MERGED" && r.merged) || (pr.state == "CLOSED" && r.closed))
        {
            return true;
        }
        if r.keep_pr && pr.state == "OPEN" {
            return false;
        }
    }
    r.inactive && now.saturating_sub(f.last_activity) > 7 * 86400
}

// GitHub timestamps are UTC RFC3339; convert the date without a local timezone dependency.
fn timestamp(s: &str) -> Option<u64> {
    let n = |a, b| s.get(a..b)?.parse::<i64>().ok();
    let (mut y, m, d, h, min, sec) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d - 1 - 719468;
    u64::try_from(days * 86400 + h * 3600 + min * 60 + sec).ok()
}

fn parse_pr(v: &Value) -> Option<LinkedPr> {
    let state = v["state"].as_str()?.to_string();
    let changed_at = v[if state == "MERGED" {
        "mergedAt"
    } else {
        "closedAt"
    }]
    .as_str()
    .and_then(timestamp)
    .unwrap_or(0);
    Some(LinkedPr {
        number: v["number"].as_u64()?,
        state,
        changed_at,
    })
}

impl Workspace {
    pub(super) fn auto_settle(&mut self, now: u64) {
        let rules = || Rules {
            inactive: self.ui["auto_settle_off"] != true,
            merged: self.ui["settle_merged_off"] != true,
            closed: self.ui["settle_closed"] == true,
            keep_pr: self.ui["settle_keep_pr"] == true,
        };
        let rules = rules();
        for t in &mut self.threads {
            if t.archived || t.meta.title.is_empty() {
                continue;
            }
            let last = std::fs::metadata(Session::path(&t.meta.id))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(t.meta.created, |d| d.as_secs());
            let facts = Facts {
                idle: t.status == Status::Idle
                    && t.pending.is_none()
                    && t.approvals.is_empty()
                    && t.dialog.is_none()
                    && t.queue.is_empty(),
                preference: t.auto_settle,
                last_activity: last,
                last_user_at: if t.last_user_at == 0 {
                    last
                } else {
                    t.last_user_at
                },
                pr: t.linked_pr_manual.as_ref().or(t.linked_pr.as_ref()),
            };
            if should_settle(facts, rules, now) {
                t.archived = true;
                t.pinned = false;
                let _ = t.save();
            }
        }
    }

    pub(super) fn refresh_thread_prs(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let threads: Vec<_> = self
            .threads
            .iter()
            .filter(|t| !t.archived)
            .map(|t| {
                (
                    t.meta.id.clone(),
                    t.meta.cwd.clone(),
                    t.linked_pr_manual.as_ref().map(|p| p.number),
                )
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut results = Vec::new();
                    for (id, cwd, manual) in threads {
                        let branch = std::process::Command::new("git")
                            .args(["branch", "--show-current"])
                            .current_dir(&cwd)
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_default();
                        if branch.is_empty() {
                            continue;
                        }
                        let output = std::process::Command::new("gh")
                            .args([
                                "pr",
                                "list",
                                "--head",
                                &branch,
                                "--state",
                                "all",
                                "--json",
                                "number,state,mergedAt,closedAt",
                                "--limit",
                                "1",
                            ])
                            .current_dir(&cwd)
                            .output();
                        let automatic = output
                            .ok()
                            .filter(|o| o.status.success())
                            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                            .map(|v| v.as_array().and_then(|a| a.first()).and_then(parse_pr));
                        let manual_pr = manual
                            .and_then(|n| {
                                std::process::Command::new("gh")
                                    .args([
                                        "pr",
                                        "view",
                                        &n.to_string(),
                                        "--json",
                                        "number,state,mergedAt,closedAt",
                                    ])
                                    .current_dir(&cwd)
                                    .output()
                                    .ok()
                            })
                            .filter(|o| o.status.success())
                            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                            .and_then(|v| parse_pr(&v));
                        results.push((id, automatic, manual_pr));
                    }
                    results
                })
                .await;
            this.update(cx, |this, cx| {
                for (id, automatic, manual) in results {
                    if let Some(t) = this
                        .threads
                        .iter_mut()
                        .find(|t| t.meta.id == id && !t.archived)
                    {
                        if let Some(pr) = automatic {
                            t.linked_pr = pr;
                        }
                        if let Some(pr) = manual {
                            if t.linked_pr_manual
                                .as_ref()
                                .is_some_and(|p| p.number == pr.number)
                            {
                                t.linked_pr_manual = Some(pr);
                            }
                        }
                        let _ = t.save();
                    }
                }
                this.auto_settle(now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::super::Act;
    use super::{should_settle, timestamp, Facts, Rules};
    use greencode_core::session::{AutoSettle, LinkedPr};
    #[test]
    fn settle_rules() {
        let open = LinkedPr {
            number: 1,
            state: "OPEN".into(),
            changed_at: 0,
        };
        let merged = LinkedPr {
            state: "MERGED".into(),
            changed_at: 20,
            ..open.clone()
        };
        let closed = LinkedPr {
            state: "CLOSED".into(),
            ..merged.clone()
        };
        let facts = || Facts {
            idle: true,
            preference: AutoSettle::Follow,
            last_activity: 0,
            last_user_at: 10,
            pr: None,
        };
        let rules = || Rules {
            inactive: true,
            merged: true,
            closed: false,
            keep_pr: false,
        };
        assert!(should_settle(facts(), rules(), 8 * 86400));
        assert!(!should_settle(
            Facts {
                preference: AutoSettle::Never,
                ..facts()
            },
            rules(),
            8 * 86400
        ));
        assert!(should_settle(
            Facts {
                pr: Some(&merged),
                ..facts()
            },
            rules(),
            100
        ));
        assert!(!should_settle(
            Facts {
                pr: Some(&closed),
                ..facts()
            },
            rules(),
            100
        ));
        assert!(!should_settle(
            Facts {
                pr: Some(&open),
                ..facts()
            },
            Rules {
                keep_pr: true,
                ..rules()
            },
            8 * 86400
        ));
        assert!(!should_settle(
            Facts {
                pr: Some(&closed),
                last_user_at: 21,
                ..facts()
            },
            Rules {
                closed: true,
                ..rules()
            },
            100
        ));
        assert!(!should_settle(
            Facts {
                idle: false,
                ..facts()
            },
            rules(),
            8 * 86400
        ));
        assert_eq!(timestamp("1970-01-01T00:00:20Z"), Some(20));
    }

    #[gpui_kit::test]
    async fn settle_palette_changes_preference(cx: &mut gpui_kit::TestAppContext) {
        let (ws, cx) = super::super::tests::open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                assert!(ws
                    .acts()
                    .iter()
                    .any(|(label, _)| label == "自動保管: 設定に従う（選択中）"));
                ws.run(Act::AutoSettle(AutoSettle::Never), window, cx);
                assert!(ws
                    .acts()
                    .iter()
                    .any(|(label, _)| label == "自動保管: しない（選択中）"));
                ws.run(Act::AutoSettle(AutoSettle::Follow), window, cx);
                assert_eq!(ws.thread().auto_settle, AutoSettle::Follow);
            })
        });
    }
}
