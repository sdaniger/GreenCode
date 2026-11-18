//! `/usage`: subscription limits of the signed-in providers.

use super::*;
use greencode_core::usage::Report;

pub(super) struct UsageView {
    /// `None` while loading.
    pub(super) reports: Option<Vec<Result<Report, String>>>,
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
        self.usage = Some(UsageView { reports: None });
        self.load_usage(cx);
        cx.notify();
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
            .w(px(540.))
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
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().flex_1())
                .child(chip("usage-refresh", "再取得", pal).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.load_usage(cx);
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
