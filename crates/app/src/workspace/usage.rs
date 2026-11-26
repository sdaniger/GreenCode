//! `/usage`: subscription limits of the signed-in providers, plus the cost and token history.

use super::*;
use greencode_core::usage::Report;
use greencode_core::usage_history::{self, Row, Totals};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum UsageTab {
    Limits,
    Cost,
    Tokens,
}

pub(super) struct UsageView {
    /// `None` while loading.
    pub(super) reports: Option<Vec<Result<Report, String>>>,
    pub(super) tab: UsageTab,
    /// Period of the history tabs: 7, 30 or 92 days.
    pub(super) days: u32,
    /// `None` while scanning.
    pub(super) history: Option<Vec<Row>>,
}

fn money(v: f64) -> String {
    if v > 0. && v < 0.01 { "<$0.01".into() } else { format!("${v:.2}") }
}

fn tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

fn limits_text(reports: &[Result<Report, String>]) -> String {
    if reports.is_empty() {
        return "サブスクリプションでログインしているプロバイダがありません。/login から Claude Pro/Max または GitHub Copilot でログインしてください。".into();
    }
    let now = now();
    let mut out = Vec::new();
    for r in reports {
        match r {
            Err(e) => out.push(e.clone()),
            Ok(r) => {
                out.push(match &r.plan { Some(p) => format!("{} · {p}", r.provider), None => r.provider.clone() });
                for l in &r.limits {
                    let reset = l.resets_at.map_or(String::new(), |at| format!("  リセット: {}", reset_in(at, now)));
                    out.push(format!("  {}  {:.0}%{reset}", l.label, l.used));
                }
            }
        }
    }
    out.join("\n")
}

fn reset_in(at: u64, now: u64) -> String {
    let secs = at.saturating_sub(now);
    match secs {
        0..=3599 => format!("{}分後", secs.div_ceil(60)),
        3600..=86399 => format!("{}時間後", secs / 3600),
        _ => format!("{}日後", secs / 86400),
    }
}

impl Workspace {
    pub(super) fn toggle_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.usage.take().is_some() {
            self.input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
            return;
        }
        self.usage = Some(UsageView { reports: None, tab: UsageTab::Limits, days: 30, history: None });
        self.load_usage(cx);
        cx.notify();
    }

    /// Palette `使用量の履歴`: the usage page on its Cost tab.
    pub(super) fn open_usage_history(&mut self, cx: &mut Context<Self>) {
        if self.usage.is_none() {
            self.usage = Some(UsageView { reports: None, tab: UsageTab::Cost, days: 30, history: None });
        }
        self.set_usage_tab(UsageTab::Cost, cx);
    }

    fn set_usage_tab(&mut self, tab: UsageTab, cx: &mut Context<Self>) {
        let Some(u) = &mut self.usage else { return };
        u.tab = tab;
        let (need_limits, need_history) = (tab == UsageTab::Limits && u.reports.is_none(), tab != UsageTab::Limits && u.history.is_none());
        if need_limits {
            self.load_usage(cx);
        }
        if need_history {
            self.load_history(cx);
        }
        cx.notify();
    }

    /// C / T / L switch tab, 1 / 2 / 3 the period; true when the key was one of them.
    pub(super) fn usage_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(u) = &mut self.usage else { return false };
        match key {
            "c" => self.set_usage_tab(UsageTab::Cost, cx),
            "t" => self.set_usage_tab(UsageTab::Tokens, cx),
            "l" => self.set_usage_tab(UsageTab::Limits, cx),
            "1" | "2" | "3" if u.tab != UsageTab::Limits => {
                u.days = [7, 30, usage_history::RETENTION_DAYS][key.parse::<usize>().unwrap_or(1) - 1];
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// `/usage-limits`: the limits as a message, without opening the page. Reuses the page's
    /// snapshot when it is already loaded rather than refreshing.
    pub(super) fn usage_limits_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(reports) = self.usage.as_ref().and_then(|u| u.reports.clone()) {
            self.notice(limits_text(&reports));
            return;
        }
        self.notice("利用上限を取得中…");
        if cfg!(test) {
            return;
        }
        let cfg = self.cfg.clone();
        let task = self.rt.spawn(async move { greencode_core::usage::all(&cfg).await });
        cx.spawn(async move |this, cx| {
            let reports = task.await.unwrap_or_else(|e| vec![Err(e.to_string())]);
            this.update(cx, |this, _| this.notice(limits_text(&reports))).ok();
        })
        .detach();
    }

    fn load_history(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let task = self.rt.spawn_blocking(usage_history::scan);
        cx.spawn(async move |this, cx| {
            let rows = task.await.unwrap_or_default();
            this.update(cx, |this, cx| {
                if let Some(u) = &mut this.usage {
                    u.history = Some(rows)
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn history_body(&self, rows: &[Row], tab: UsageTab, days: u32, pal: Pal) -> Vec<AnyElement> {
        let cost = tab == UsageTab::Cost;
        let metric = |t: &Totals| if cost { t.cost } else { t.tokens() as f64 };
        let show = |t: &Totals| if cost { money(t.cost) } else { tokens(t.tokens()) };
        let s = usage_history::summarize(rows, &usage_history::Settings::from_ui(&self.ui), days, (now() / 86400) as u32);
        if s.total.tokens() == 0 && s.total.cost == 0. {
            return vec![div().text_xs().text_color(pal.muted).child("この期間の履歴はありません。").into_any_element()];
        }
        let t = &s.total;
        let peak = s.days.iter().map(|d| metric(&d.1)).fold(0., f64::max).max(f64::EPSILON);
        let (first, last) = (usage_history::month_day(s.days[0].0), usage_history::month_day(s.days[s.days.len() - 1].0));
        let mut out = vec![
            div().flex().items_baseline().gap_2()
                .child(div().text_xl().font_weight(FontWeight::MEDIUM).child(show(t)))
                .child(div().text_xs().text_color(pal.muted).child(if cost { "API 換算の推定額（請求額ではありません）" } else { "合計トークン" }))
                .into_any_element(),
            div().text_xs().text_color(pal.muted)
                .child(format!("入力 {} · 出力 {} · キャッシュ読込 {} · キャッシュ書込 {}", tokens(t.input), tokens(t.output), tokens(t.cache_read), tokens(t.cache_write)))
                .into_any_element(),
        ];
        if t.savings > 0. {
            out.push(div().text_xs().text_color(pal.added).child(format!("キャッシュ読込による節約 ≈ {}", money(t.savings))).into_any_element());
        }
        out.push(
            div().flex().items_end().gap(px(1.)).h(px(64.)).children(s.days.iter().map(|(_, d)| {
                let h = (metric(d) / peak * 64.).max(if metric(d) > 0. { 1. } else { 0. });
                div().flex_1().h(px(h as f32)).bg(pal.accent).rounded_t_sm()
            })).into_any_element(),
        );
        out.push(
            div().flex().justify_between().text_xs().text_color(pal.muted)
                .child(format!("{}/{}", first.0, first.1)).child(format!("{}/{}", last.0, last.1)).into_any_element(),
        );
        out.push(div().text_xs().text_color(pal.muted).mt_1().child("モデル別").into_any_element());
        let top = s.models.iter().map(|m| metric(&m.1)).fold(0., f64::max).max(f64::EPSILON);
        for (model, t) in s.models.iter().take(10) {
            let value = if cost && t.unpriced && t.cost == 0. { "価格未設定".to_string() } else { show(t) };
            out.push(
                div().flex().items_center().gap_2().text_xs()
                    .child(div().w(px(200.)).truncate().child(model.clone()))
                    .child(div().flex_1().h(px(6.)).rounded_full().bg(pal.line)
                        .child(div().h_full().rounded_full().bg(pal.accent).w(relative((metric(t) / top).clamp(0., 1.) as f32))))
                    .child(div().w(px(70.)).text_right().child(value))
                    .into_any_element(),
            );
        }
        if cost && s.models.iter().any(|m| m.1.unpriced) {
            out.push(div().text_xs().text_color(pal.muted).child("価格のないモデルは ui.json の model_prices（input / output / cache_read / cache_write / map_to）で設定できます。").into_any_element());
        }
        out
    }

    fn load_usage(&mut self, cx: &mut Context<Self>) {
        if let Some(u) = &mut self.usage {
            u.reports = None
        }
        if cfg!(test) {
            return;
        }
        let cfg = self.cfg.clone();
        let task = self
            .rt
            .spawn(async move { greencode_core::usage::all(&cfg).await });
        cx.spawn(async move |this, cx| {
            let reports = task.await.unwrap_or_else(|e| vec![Err(e.to_string())]);
            this.update(cx, |this, cx| {
                if let Some(u) = &mut this.usage {
                    u.reports = Some(reports)
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn usage_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let view = self.usage.as_ref()?;
        let mut col = div()
            .w(px(600.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child("利用状況");
        let tab_chip = |id: &'static str, label: &'static str, tab: UsageTab, cx: &mut Context<Self>| {
            chip(id, label, pal)
                .when(view.tab == tab, |c| c.bg(pal.selected).text_color(pal.fg))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_usage_tab(tab, cx)))
        };
        let mut tabs = div().flex().items_center().gap_1()
            .child(tab_chip("usage-tab-cost", "コスト  C", UsageTab::Cost, cx))
            .child(tab_chip("usage-tab-tokens", "トークン  T", UsageTab::Tokens, cx))
            .child(tab_chip("usage-tab-limits", "上限  L", UsageTab::Limits, cx));
        if view.tab != UsageTab::Limits {
            tabs = tabs.child(div().flex_1());
            for (i, d) in [7u32, 30, usage_history::RETENTION_DAYS].into_iter().enumerate() {
                tabs = tabs.child(
                    chip(("usage-days", i), format!("{d}日  {}", i + 1), pal)
                        .when(view.days == d, |c| c.bg(pal.selected).text_color(pal.fg))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(u) = &mut this.usage { u.days = d }
                            cx.notify();
                        })),
                );
            }
        }
        col = col.child(tabs);
        if view.tab != UsageTab::Limits {
            match &view.history {
                None => col = col.child(div().text_xs().text_color(pal.muted).child("集計中…")),
                Some(rows) => col = col.children(self.history_body(rows, view.tab, view.days, pal)),
            }
        } else {
        match &view.reports {
            None => col = col.child(div().text_xs().text_color(pal.muted).child("取得中…")),
            Some(r) if r.is_empty() => col = col.child(div().text_xs().text_color(pal.muted).child("サブスクリプションでログインしているプロバイダがありません。/login から Claude Pro/Max または GitHub Copilot でログインしてください。")),
            Some(reports) => {
                let now = now();
                for report in reports {
                    match report {
                        Err(e) => col = col.child(div().text_xs().text_color(pal.removed).child(e.clone())),
                        Ok(report) => {
                            let title = match &report.plan { Some(p) => format!("{} · {p}", report.provider), None => report.provider.clone() };
                            col = col.child(div().font_weight(FontWeight::MEDIUM).child(title));
                            for l in &report.limits {
                                let fill = if l.used >= 95. { pal.removed } else if l.used >= 80. { pal.warn } else { pal.accent };
                                let mut row = div().flex().items_center().gap_2().text_xs()
                                    .child(div().w(px(140.)).child(l.label.clone()))
                                    .child(div().flex_1().h(px(6.)).rounded_full().bg(pal.line)
                                        .child(div().h_full().rounded_full().bg(fill).w(relative((l.used / 100.).clamp(0., 1.) as f32))))
                                    .child(div().w(px(40.)).child(format!("{:.0}%", l.used)));
                                if let Some(at) = l.resets_at {
                                    row = row.child(div().w(px(110.)).text_color(pal.muted).child(format!("リセット: {}", reset_in(at, now))));
                                }
                                col = col.child(row);
                            }
                        }
                    }
                }
            }
        }
        }
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().flex_1())
                .child(chip("usage-refresh", "再取得", pal).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        match this.usage.as_ref().map(|u| u.tab) {
                            Some(UsageTab::Limits) => this.load_usage(cx),
                            _ => this.load_history(cx),
                        }
                        cx.notify();
                    },
                )))
                .child(chip("usage-close", "閉じる  esc", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_usage(window, cx)),
                )),
        );
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }
}
