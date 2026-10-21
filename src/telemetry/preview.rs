//! Display-only query summaries; these strings are not executable queries.
const BUDGET: usize = 100;

pub(super) fn truncate(value: &str, budget: usize) -> String {
    if value.len() <= budget {
        return value.to_owned();
    }
    if budget < '…'.len_utf8() {
        return String::new();
    }
    let mut end = budget - '…'.len_utf8();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

pub(super) fn summarize(query: &str) -> String {
    if query.len() <= BUDGET {
        return query.to_owned();
    }
    // Split only outside quotes so text selectors remain recognizable.
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in query.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if Some(character) == quote {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none() && character.is_whitespace() {
            if start < index {
                tokens.push(&query[start..index]);
            }
            start = index + character.len_utf8();
        }
    }
    if start < query.len() {
        tokens.push(&query[start..]);
    }
    let mut compact: Vec<String> = tokens.iter().map(|token| compact_token(token)).collect();
    if compact.join(" ").len() <= BUDGET {
        return compact.join(" ");
    }
    // Keep behavior controls ahead of output controls when there is no room.
    let mut omitted = false;
    for prefix in ["limit:", "offset:", "lang:"] {
        compact.retain(|token| {
            if token.starts_with(prefix) {
                omitted = true;
                false
            } else {
                true
            }
        });
        if compact.join(" ").len() + 4 <= BUDGET {
            break;
        }
    }
    let joined = compact.join(" ");
    if joined.len() + if omitted { 4 } else { 0 } <= BUDGET {
        return if omitted {
            format!("{joined} …")
        } else {
            joined
        };
    }
    // Reserve complete behavior controls instead of cutting them in half.
    let controls: Vec<&str> = compact
        .iter()
        .filter(|token| token.starts_with("match:") || token.starts_with("wait:"))
        .map(String::as_str)
        .collect();
    let tail = controls.join(" ");
    let head = compact
        .iter()
        .filter(|token| !token.starts_with("match:") && !token.starts_with("wait:"))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    if tail.len() + 8 < BUDGET && !tail.is_empty() {
        format!("{} {tail}", truncate(&head, BUDGET - tail.len() - 1))
    } else {
        truncate(&joined, BUDGET)
    }
}

fn compact_token(token: &str) -> String {
    let Some((key, value)) = token.split_once(':') else {
        return truncate(token, 55);
    };
    let compact = if matches!(key.trim_start_matches('-'), "path" | "file") && value.len() > 25 {
        let parts: Vec<&str> = value.rsplitn(3, '/').collect();
        if parts.len() == 3 {
            format!("…/{}/{}", parts[1], parts[0])
        } else {
            truncate(value, 30)
        }
    } else if value.starts_with('"') || value.starts_with('\'') {
        if value.len() > 42 {
            let first = truncate(value, 25);
            let mut start = value.len().saturating_sub(14);
            while !value.is_char_boundary(start) {
                start += 1;
            }
            format!("{first} {}", &value[start..])
        } else {
            value.to_owned()
        }
    } else if !matches!(
        key,
        "match" | "wait" | "project" | "limit" | "offset" | "lang"
    ) {
        let (name, parameters) = value
            .split_once('(')
            .map_or((value, ""), |(name, _)| (name, "(…)"));
        let parts: Vec<&str> = name.rsplitn(3, '.').collect();
        let name = if parts.len() == 3 {
            format!("…{}.{}", parts[1], parts[0])
        } else {
            name.to_owned()
        };
        format!("{name}{parameters}")
    } else {
        value.to_owned()
    };
    format!("{key}:{compact}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_short_queries_and_unicode_boundaries() {
        let short = "text:\"文字列\" path:src/**";
        assert_eq!(summarize(short), short);
        let long = format!(
            "text:\"{}\" match:loose wait:complete limit:200",
            "🦀文字列".repeat(80)
        );
        let preview = summarize(&long);
        assert!(preview.len() <= BUDGET);
        assert!(preview.contains("…"));
        assert!(preview.contains("match:loose"));
        assert!(preview.contains("wait:complete"));
    }
    #[test]
    fn keeps_target_and_useful_filters() {
        let query = "calls:Long.Namespace.Parser.Parse(string,CancellationToken) path:src/Services/** project:Backend lang:cs limit:200 match:loose wait:complete";
        let preview = summarize(query);
        assert!(preview.len() <= BUDGET);
        assert!(preview.contains("Parser.Parse"));
        assert!(preview.contains("Services"));
        assert!(preview.contains("wait:complete"));
    }
}
