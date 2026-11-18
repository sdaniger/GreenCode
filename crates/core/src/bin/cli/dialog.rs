use serde_json::{json, Value};

pub struct Dialog {
    request: Value,
    pub prompts: Vec<String>,
    pub answers: Vec<String>,
}

fn multi_select(question: &Value) -> bool {
    question["multiSelect"] == true || question["multi"] == true
}

impl Dialog {
    pub fn new(request: Value) -> Option<Self> {
        let prompts = if request["method"] == "ask" {
            let questions = request["questions"].as_array()?;
            if questions.is_empty() || questions.iter().any(|q| !q["id"].is_string()) { return None; }
            questions.iter().map(|q| {
                let mut text = q["question"].as_str().unwrap_or_default().to_owned();
                if let Some(options) = q["options"].as_array() {
                    for (i, option) in options.iter().enumerate() {
                        text.push_str(&format!("\n{}: {}", i + 1, option["label"].as_str().unwrap_or_default()));
                    }
                }
                if q["options"].as_array().is_some_and(|options| !options.is_empty()) {
                    text.push_str(if multi_select(q) { "\nEnter option numbers separated by commas, or free text." } else { "\nEnter an option number or free text." });
                }
                text
            }).collect()
        } else {
            let mut text = format!("{}\n{}", request["title"].as_str().unwrap_or("Agent question"), request["message"].as_str().unwrap_or_default());
            if let Some(options) = request["options"].as_array() {
                for (i, option) in options.iter().enumerate() {
                    text.push_str(&format!("\n{}: {}", i + 1, option.as_str().unwrap_or_default()));
                }
            }
            if request["method"] == "confirm" { text.push_str("\n[y]es / [n]o"); }
            vec![text]
        };
        Some(Self { request, prompts, answers: Vec::new() })
    }

    pub fn secret(&self) -> bool { self.request["secret"] == true }

    pub fn finish(self) -> Value {
        if self.prompts.len() != self.answers.len() { return json!({"cancelled": true}); }
        if let Some(questions) = self.request["questions"].as_array().filter(|_| self.request["method"] == "ask") {
            let answers: Vec<_> = questions.iter().zip(&self.answers).map(|(q, answer)| {
                let value = answer.trim();
                let selected: Option<Vec<_>> = value.split(',').map(|s| {
                    let i = s.trim().parse::<usize>().ok()?.checked_sub(1)?;
                    q["options"].get(i)?["label"].as_str()
                }).collect();
                let mut result = json!({"id":q["id"], "selectedOptions":[]});
                if let Some(selected) = selected.filter(|v| multi_select(q) || v.len() == 1) {
                    result["selectedOptions"] = json!(selected);
                } else if !value.is_empty() {
                    result["customInput"] = json!(value);
                }
                result
            }).collect();
            return json!({"answers": answers});
        }
        let value = self.answers[0].trim_end();
        match self.request["method"].as_str() {
            Some("confirm") => json!({"confirmed": matches!(value, "y" | "yes")}),
            Some("select") => value.parse::<usize>().ok().and_then(|n| n.checked_sub(1))
                .and_then(|i| self.request["options"].get(i)).and_then(Value::as_str)
                .map_or_else(|| json!({"cancelled":true}), |v| json!({"value":v})),
            _ => json!({"value": value}),
        }
    }
}
