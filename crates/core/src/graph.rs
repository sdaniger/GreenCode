//! `autoGraph`: chart the last numeric Markdown table of a final reply as a bar chart in the
//! transcript, colored with the terminal theme's accent.

/// The last table in `markdown` with a label column and at least one numeric column and two rows.
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Index of the numeric column charted.
    pub column: usize,
}

/// Find the last chartable table; fenced code blocks are skipped.
pub fn last_numeric_table(markdown: &str) -> Option<Table> {
    let mut tables: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            if !current.is_empty() { tables.push(std::mem::take(&mut current)); }
            continue;
        }
        if fenced { continue; }
        let trimmed = line.trim();
        if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
            current.push(trimmed.to_string());
        } else if !current.is_empty() {
            tables.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() { tables.push(current); }
    tables.into_iter().rev().find_map(parse)
}

fn cells(row: &str) -> Vec<String> {
    row.trim().trim_matches('|').split('|').map(|cell| cell.trim().trim_matches('`').trim().to_string()).collect()
}

fn numeric(cell: &str) -> Option<f64> {
    let cleaned: String = cell.chars().filter(|c| !matches!(c, ',' | '$' | '%' | '_')).collect();
    cleaned.trim_end_matches(['k', 'K', 'M']).parse::<f64>().ok().map(|value| {
        if cleaned.ends_with(['k', 'K']) { value * 1e3 } else if cleaned.ends_with('M') { value * 1e6 } else { value }
    })
}

fn parse(rows: Vec<String>) -> Option<Table> {
    if rows.len() < 4 { return None; }
    let headers = cells(&rows[0]);
    if !rows[1].chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) { return None; }
    let body: Vec<Vec<String>> = rows[2..].iter().map(|row| cells(row)).filter(|row| row.len() == headers.len()).collect();
    if body.len() < 2 { return None; }
    let column = (1..headers.len()).find(|&c| body.iter().all(|row| numeric(&row[c]).is_some()))?;
    Some(Table { headers, rows: body, column })
}

/// Bar lines for the chart: one row per label, width `max`, scaled to the largest value.
pub fn chart(table: &Table, width: usize, ascii: bool) -> Vec<String> {
    let values: Vec<f64> = table.rows.iter().filter_map(|row| numeric(&row[table.column])).collect();
    let max = values.iter().copied().fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
    let label_width = table.rows.iter().map(|row| row[0].chars().count()).max().unwrap_or(0).min(16);
    let (full, empty) = if ascii { ('#', '.') } else { ('█', '·') };
    let bar_width = width.saturating_sub(label_width + 9).max(4);
    table.rows.iter().map(|row| {
        let Some(value) = numeric(&row[table.column]) else { return String::new() };
        let cells = ((value / max) * bar_width as f64).round().max(if value > 0.0 { 1.0 } else { 0.0 }) as usize;
        let mut label: String = row[0].chars().take(label_width).collect();
        while label.chars().count() < label_width { label.push(' '); }
        format!("{label} {}{}  {}", full.to_string().repeat(cells), empty.to_string().repeat(bar_width - cells), short(value))
    }).filter(|line| !line.is_empty()).collect()
}

fn short(value: f64) -> String {
    if value >= 1e6 { format!("{:.1}M", value / 1e6) } else if value >= 1e3 { format!("{:.1}k", value / 1e3) } else if value.fract() == 0.0 { format!("{value:.0}") } else { format!("{value:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charts_the_last_table_and_ignores_prose_and_fences() {
        let text = "| day | ms |\n|---|---|\n| mon | 10 |\n| tue | 30 |\n\nlater\n| name | total |\n|---|---|\n| a | 1,200 |\n| b | 3k |\n";
        let table = last_numeric_table(text).unwrap();
        assert_eq!(table.column, 1);
        let lines = chart(&table, 40, true);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("a "), "{lines:?}");
        assert!(lines[1].contains("###"), "{lines:?}");
        assert!(lines[1].contains("3.0k"), "{lines:?}");
        assert!(last_numeric_table("no table").is_none());
        assert!(last_numeric_table("| a | b |\n|---|---|\n| x | y |\n| z | w |").is_none());
    }
}
