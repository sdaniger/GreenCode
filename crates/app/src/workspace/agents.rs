use super::{spawn_external, Context, Window, Workspace};
use greencode_core::agents::{self, AGENTS};

impl Workspace {
    pub(super) fn refresh_agent_versions(&mut self, cx: &mut Context<Self>) {
        self.agent_versions.clear();
        self.agent_versions_generation += 1;
        let generation = self.agent_versions_generation;
        if cfg!(test) {
            return;
        }
        let task = self.rt.spawn(async {
            let mut jobs = tokio::task::JoinSet::new();
            for agent in AGENTS {
                jobs.spawn(async move { (agent.name, agents::version(agent.binary).await) });
            }
            let mut versions = std::collections::BTreeMap::new();
            while let Some(Ok((name, version))) = jobs.join_next().await {
                versions.insert(name, version);
            }
            versions
        });
        cx.spawn(async move |this, cx| {
            if let Ok(versions) = task.await {
                this.update(cx, |this, cx| {
                    if this.agent_versions_generation == generation && this.settings.is_some() {
                        this.agent_versions = versions;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn install_agent(
        &mut self,
        name: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = AGENTS.iter().find(|a| a.name == name) else {
            return;
        };
        if agent.install.is_empty() {
            spawn_external("/usr/bin/open", &[agent.docs]);
            return;
        }
        self.settings = None;
        if !self.terminal_visible {
            self.open_terminal(window, cx);
        }
        if self.active_terminal().is_none() {
            return;
        }
        if cfg!(test) {
            self.active_terminal_mut()
                .unwrap()
                .lines
                .push(format!("$ {}", agent.install).into());
        } else {
            self.write_terminal(format!("{}\n", agent.install).into_bytes());
        }
        cx.notify();
    }
}
