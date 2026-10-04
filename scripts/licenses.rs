use regex::Regex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// A source document or notice extract, before display formatting.
#[derive(Serialize)]
pub struct Notice {
    pub name: String,
    pub source: String,
    pub license: String,
    pub text: String,
    /// Editorial context, displayed outside the upstream text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

static GRANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:Permission\s+is\s+hereby\s+granted|Redistribution and use|Permission to use, copy)").unwrap()
});

fn key(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Shared<'a> {
    notice: &'a Notice,
    attribution: String,
    terms: String,
    title: &'static str,
}

/// Only split a single complete grant preceded by plain attribution text.
/// Compound documents, exceptions, and unfamiliar forms remain intact.
fn simple<'a>(notice: &'a Notice, apache: &str) -> Option<Shared<'a>> {
    if notice.note.is_some() {
        return None;
    }
    if notice.text.split_whitespace().eq(apache.split_whitespace()) {
        return Some(Shared {
            notice,
            attribution: String::new(),
            terms: apache.trim().into(),
            title: "Apache License 2.0",
        });
    }
    let title = match notice.license.as_str() {
        "MIT" => "MIT License",
        "ISC" => "ISC License",
        "BSD-2-Clause" => "BSD 2-Clause License",
        "BSD-3-Clause" => "BSD 3-Clause License",
        "0BSD" => "Zero-Clause BSD License",
        "" => "Native source license terms",
        _ => return None,
    };
    let text = notice
        .text
        .lines()
        .map(|line| {
            if line.trim() == "//" {
                ""
            } else {
                line.strip_prefix("// ").unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    static HEADINGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?mi)^[ \t]*(?:(?:The )?MIT License(?: \(MIT\))?|ISC License|BSD(?: [23]-Clause)? License(?: \(https?://[^)]+\))?|[-=_]{3,})[ \t]*$").unwrap()
    });
    let text = HEADINGS.replace_all(&text, "");
    let mut grants = GRANT.find_iter(&text);
    let grant = grants.next()?;
    if grants.next().is_some() {
        return None;
    }
    let attribution = text[..grant.start()].trim();
    if !attribution.is_empty() && !attribution.contains("Copyright")
        || [
            "License",
            "license",
            "Permission",
            "permission",
            "Redistribution",
            "redistribution",
        ]
        .iter()
        .any(|word| attribution.contains(word))
    {
        return None;
    }
    let body = &text[grant.start()..];
    static END: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:DEALINGS\s+IN\s+THE\s+SOFTWARE\.|PERFORMANCE\s+OF\s+THIS\s+SOFTWARE\.|POSSIBILITY\s+OF\s+SUCH\s+DAMAGE\.|provided\s+that\s+this\s+notice\s+is\s+preserved\.)").unwrap()
    });
    let end = END.find(body)?.end();
    let tail = body[end..].trim();
    let lower_tail = tail.to_ascii_lowercase();
    if [
        "license",
        "permission",
        "condition",
        "warranty",
        "redistribution",
    ]
    .iter()
    .any(|word| lower_tail.contains(word))
    {
        return None;
    }
    if !tail.is_empty()
        && ![
            "Based on:",
            "Based on NetBSD:",
            "You can contact the author at",
            "Borrowed from FreeBSD",
            "Optimized by ",
            "The argument reduction and testing",
        ]
        .iter()
        .any(|prefix| tail.starts_with(prefix))
    {
        return None;
    }
    Some(Shared {
        notice,
        attribution: [attribution, tail]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        terms: body[..end].trim().into(),
        title,
    })
}

fn heading(output: &mut String, title: &str, underline: char) {
    output.push_str(title);
    output.push('\n');
    output.extend(std::iter::repeat_n(underline, title.chars().count()));
    output.push_str("\n\n");
}

fn write_line<'a>(output: &mut String, words: impl Iterator<Item = &'a str>, prefix: &str) {
    output.push_str(prefix);
    let mut width = prefix.chars().count();
    for (index, word) in words.enumerate() {
        let length = word.chars().count();
        if index != 0 {
            if width + 1 + length > 76 {
                output.push('\n');
                output.push_str(prefix);
                width = prefix.chars().count();
            } else {
                output.push(' ');
                width += 1;
            }
        }
        output.push_str(word);
        width += length;
    }
    output.push('\n');
}

fn names<'a>(output: &mut String, values: impl IntoIterator<Item = &'a str>) {
    let values = values.into_iter().collect::<Vec<_>>().join(", ");
    write_line(output, values.split_inclusive(", ").map(str::trim), "  ");
}

fn verbatim(output: &mut String, text: &str) {
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');
}

fn apache_core(text: &str) -> String {
    key(text.split("END OF TERMS AND CONDITIONS").next().unwrap())
}

pub fn render(notices: &[Notice], apache: &str, own_license: &str) -> String {
    let mut shared: BTreeMap<String, Vec<Shared<'_>>> = BTreeMap::new();
    let mut documents: BTreeMap<&str, Vec<&Notice>> = BTreeMap::new();
    for notice in notices {
        if let Some(entry) = simple(notice, apache) {
            shared.entry(key(&entry.terms)).or_default().push(entry);
        } else {
            documents.entry(&notice.text).or_default().push(notice);
        }
    }
    let own_terms = GRANT
        .find(own_license)
        .map(|grant| key(&own_license[grant.start()..]));
    let mut sections: Vec<_> = shared
        .iter()
        .map(|(terms, entries)| {
            let title = entries.iter().map(|entry| entry.title).min().unwrap();
            ((Some(terms) != own_terms.as_ref(), title, terms), entries)
        })
        .collect();
    sections.sort_unstable_by_key(|(order, _)| *order);
    let mut documents: Vec<_> = documents.into_iter().collect();
    documents.sort_by_key(|(_, records)| {
        records
            .iter()
            .map(|notice| (&notice.name, &notice.source))
            .min()
            .unwrap()
    });

    let mut output = String::new();
    heading(&mut output, "Sigla", '=');
    verbatim(&mut output, own_license);
    output.push('\n');
    heading(&mut output, "Third-party licenses", '=');
    output.push_str(
        "The opening license applies to original Sigla code. Third-party code\n\
         retains its own licenses and notices.\n\n\
         Each component's attribution and its identified license text together form\n\
         its notice. Shared terms apply separately to each listed component and its\n\
         applicable copyright holders. This organization does not change the terms.\n\n\
         Complete documents in the upstream notices section retain their original\n\
         order and wording. Extracts and text renderings are labeled separately.\n\
         Paths and references within reproduced material refer to the upstream\n\
         source distribution. Some documents describe several licenses or choices.\n\n\n",
    );
    heading(&mut output, "Shared license terms", '=');
    for (index, ((_, title, _), entries)) in sections.into_iter().enumerate() {
        heading(&mut output, &format!("{}. {title}", index + 1), '-');
        render_shared(&mut output, entries);
    }
    heading(&mut output, "Upstream notices", '=');
    let apache_core = apache_core(apache);
    for (index, (text, records)) in documents.into_iter().enumerate() {
        render_document(&mut output, index + 1, text, &records, &apache_core);
    }
    output
}

fn render_shared(output: &mut String, entries: &[Shared<'_>]) {
    let mut by_component: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in entries {
        by_component
            .entry(&entry.notice.name)
            .or_default()
            .push(&entry.attribution);
    }
    let mut by_attribution: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (name, records) in by_component {
        by_attribution
            .entry(compact_attributions(records))
            .or_default()
            .push(name);
    }
    let mut attributions: Vec<_> = by_attribution.into_iter().collect();
    attributions.sort_unstable_by(|a, b| a.1.cmp(&b.1));

    output
        .push_str("The terms following these attributions apply separately to each component.\n\n");
    for (attribution, components) in attributions {
        names(output, components);
        for line in attribution.lines() {
            write_line(output, line.split_whitespace(), "    ");
        }
        output.push('\n');
    }
    output.push_str("License terms\n\n");
    verbatim(
        output,
        entries
            .iter()
            .map(|entry| entry.terms.as_str())
            .min()
            .unwrap(),
    );
    output.push('\n');
}

fn render_document(
    output: &mut String,
    number: usize,
    text: &str,
    records: &[&Notice],
    standard_apache: &str,
) {
    let components: BTreeSet<_> = records.iter().map(|notice| notice.name.as_str()).collect();
    heading(
        output,
        &format!("{number}. {}", components.first().unwrap()),
        '-',
    );
    if components.len() > 1 {
        names(output, components);
    }
    let sources: BTreeSet<_> = records
        .iter()
        .map(|notice| notice.source.as_str())
        .filter(|source| !source.is_empty())
        .collect();
    for source in sources {
        write_line(output, format!("Source: {source}").split_whitespace(), "  ");
    }
    output.push('\n');
    let notes: BTreeSet<_> = records
        .iter()
        .filter_map(|notice| notice.note.as_deref())
        .collect();
    for note in notes {
        write_line(output, note.split_whitespace(), "");
        output.push('\n');
    }
    if text.trim_start().starts_with("Apache License") && apache_core(text) != standard_apache {
        output.push_str(
            "This upstream license text differs substantively from standard Apache 2.0.\n\
             It is reproduced as supplied; its upstream label does not resolve those\n\
             differences.\n\n",
        );
    }
    output.push_str("--- Begin upstream text ---\n\n");
    verbatim(output, text);
    output.push_str("--- End upstream text ---\n\n\n");
}

fn files(directory: &Path, recursive: bool) -> io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if recursive {
                result.extend(files(&entry.path(), true)?);
            }
        } else {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(result)
}

pub fn package_notices(root: &Path, name: &str) -> io::Result<Vec<Notice>> {
    let mut notices = Vec::new();
    for file in files(root, false)? {
        let filename = file.file_name().unwrap().to_string_lossy();
        let upper = filename.to_ascii_uppercase();
        if upper == "NOTICE" || upper.starts_with("NOTICE.") || upper.starts_with("NOTICE-") {
            notices.push(Notice {
                name: name.into(),
                source: filename.into_owned(),
                license: String::new(),
                text: fs::read_to_string(file)?,
                note: None,
            });
        }
    }
    Ok(notices)
}

/// AWS-LC's inherited Rust code has ISC owners absent from its top-level LICENSE.
/// Keep each source attribution; the renderer combines repeated owners and terms.
pub fn isc_source_notices(root: &Path, name: &str) -> io::Result<Vec<Notice>> {
    let license = fs::read_to_string(root.join("LICENSE"))?;
    let terms = license
        .split_once("\nISC license\n")
        .and_then(|(_, section)| {
            section
                .find("Permission to use, copy")
                .map(|start| &section[start..])
        })
        .ok_or_else(|| io::Error::other("Missing ISC terms in upstream LICENSE"))?;
    let mut notices = Vec::new();
    for file in files(&root.join("src"), true)? {
        if file.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&file)?;
        let header: Vec<_> = text
            .lines()
            .map_while(|line| line.strip_prefix("//"))
            .map(str::trim_start)
            .collect();
        let Some(end) = header
            .iter()
            .position(|line| *line == "SPDX-License-Identifier: ISC")
        else {
            continue;
        };
        notices.push(Notice {
            name: format!("{name} (inherited Rust code)"),
            source: format!(
                "{} (ISC attribution excerpt; terms from LICENSE)",
                file.strip_prefix(root).unwrap().display()
            ),
            license: "ISC".into(),
            text: format!("{}\n\n{terms}", header[..end].join("\n")),
            note: None,
        });
    }
    Ok(notices)
}

pub fn native_notices(root: &Path, library: &str, name: &str) -> io::Result<Vec<Notice>> {
    let license = match library {
        "musl" => fs::read_to_string(root.join("COPYRIGHT"))?
            .split(&"-".repeat(70))
            .nth(1)
            .expect("Missing musl license")
            .trim()
            .to_owned(),
        "zstd" => fs::read_to_string(root.join("LICENSE"))?,
        _ => String::new(),
    };
    let mut notices = Vec::new();
    if !license.is_empty() {
        notices.push(Notice {
            name: name.into(),
            source: if library == "musl" {
                "COPYRIGHT (MIT section)"
            } else {
                "LICENSE"
            }
            .into(),
            license: String::new(),
            text: license.clone(),
            note: None,
        });
    }
    let directory = root.join(match library {
        "libarchive" => "libarchive",
        "zstd" => "lib",
        _ => "src",
    });
    static COMMENTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*(.*?)\*/").unwrap());
    static COPYRIGHT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?mi)^Copyright[^\n]*").unwrap());
    let terms = GRANT.find(&license).map(|grant| &license[grant.start()..]);
    for file in files(&directory, library != "libarchive")? {
        let relative = file.strip_prefix(&directory).unwrap().to_string_lossy();
        if !matches!(
            file.extension().and_then(|ext| ext.to_str()),
            Some("c" | "h" | "s" | "S")
        ) {
            continue;
        }
        if library == "libarchive" && relative.contains("windows") {
            continue;
        }
        // Release builds target x86-64. Keep generic notices, including crypt.
        if library == "musl" && relative.matches('/').count() > 1 && !relative.contains("/x86_64/")
        {
            continue;
        }
        for comment in COMMENTS.captures_iter(&fs::read_to_string(&file)?) {
            let text = comment[1]
                .strip_prefix('-')
                .unwrap_or(&comment[1])
                .lines()
                .map(|line| line.trim().strip_prefix('*').unwrap_or(line.trim()).trim())
                .collect::<Vec<_>>()
                .join("\n");
            if !COPYRIGHT.is_match(&text) {
                continue;
            }
            let text = text.trim();
            let text = if GRANT.is_match(text) {
                text.to_owned()
            } else if text.contains("SPDX-License-Identifier: BSD-3-Clause")
                || text.contains("SPDX-License-Identifier: MIT")
                || text.contains("licensed under standard MIT license")
                || library == "zstd" && text.contains("licensed under both the BSD-style license")
            {
                let owners = COPYRIGHT
                    .find_iter(text)
                    .map(|found| {
                        found
                            .as_str()
                            .replace(", licensed under standard MIT license", "")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{owners}\n\n{}",
                    terms.expect("Missing referenced native license")
                )
            } else {
                continue;
            };
            notices.push(Notice {
                name: name.into(),
                source: format!(
                    "{} (source-file notice block)",
                    file.strip_prefix(root).unwrap().display()
                ),
                license: String::new(),
                text,
                note: None,
            });
        }
    }
    Ok(notices)
}

fn compact_attributions<'a>(attributions: impl IntoIterator<Item = &'a str>) -> String {
    static OWNER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^Copyright[ \t]*(?:\([cC]\)|©)?[ \t]*(?P<years>\d{4}(?:[ \t]*-[ \t]*\d{4})?(?:[ \t]*,[ \t]*\d{4}(?:[ \t]*-[ \t]*\d{4})?)*)(?:[, \t]+|\r?\n)(?:by )?(?P<owner>[^\d\s][^\r\n]*)").unwrap()
    });
    static RESERVED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\s*All rights reserved\.?").unwrap());
    let mut owners: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut other = BTreeSet::new();
    let mut reserved = false;
    for attribution in attributions {
        reserved |= RESERVED.is_match(attribution);
        let text = RESERVED.replace_all(attribution, "");
        let text = OWNER.replace_all(&text, |found: &regex::Captures<'_>| {
            let owner = key(&found["owner"]).trim_end_matches('.').to_owned();
            let years = owners.entry(owner).or_default();
            for dates in found["years"].split(',') {
                let (start, end) = dates.split_once('-').unwrap_or((dates, dates));
                years.extend(
                    start.trim().parse::<u32>().unwrap()..=end.trim().parse::<u32>().unwrap(),
                );
            }
            ""
        });
        if !text.trim().is_empty() {
            other.insert(text.trim().to_owned());
        }
    }
    let mut lines = Vec::new();
    for (owner, years) in owners {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for year in years {
            if let Some(last) = ranges.last_mut()
                && last.1 + 1 == year
            {
                last.1 = year;
            } else {
                ranges.push((year, year));
            }
        }
        let dates = ranges
            .into_iter()
            .map(|(start, end)| {
                if start == end {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Copyright (c) {dates} {owner}"));
    }
    lines.extend(other);
    if reserved {
        lines.push("All rights reserved.".into());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const APACHE: &str = "Apache License\nAn original condition.\nEND OF TERMS AND CONDITIONS\nAPPENDIX: an example.";
    const MIT: &str = "Permission is hereby granted, under these conditions.\nThe above copyright notice must remain.\nDEALINGS IN THE SOFTWARE.";

    fn notice(name: &str, text: impl Into<String>) -> Notice {
        Notice {
            name: name.into(),
            source: String::new(),
            license: "MIT".into(),
            text: text.into(),
            note: None,
        }
    }

    #[test]
    fn sharing_preserves_owners_and_distinct_terms() {
        let own = format!("Copyright Application owner\n{MIT}");
        let altered = MIT.replace("these conditions", "different conditions");
        let first = format!("Copyright First owner\n{MIT}\nBased on: original.c");
        let mut notices = [
            notice("alpha", &first),
            notice(
                "beta",
                format!(
                    "// Copyright Second owner\n//\n// {}",
                    MIT.replace('\n', "\n// ")
                ),
            ),
            notice("gamma", format!("Copyright Third owner\n{altered}")),
            notice("alpha", &first),
        ];
        let output = render(&notices, APACHE, &own);
        let (_, third_party) = output.split_once(&own).unwrap();
        assert_eq!(third_party.matches(MIT).count(), 1);
        assert!(third_party.contains(&altered));
        assert!(!third_party.contains("Application owner"));
        let alpha = third_party.find("alpha").unwrap();
        let beta = third_party.find("beta").unwrap();
        assert!(third_party[alpha..beta].contains("First owner"));
        assert!(third_party[alpha..beta].contains("Based on: original.c"));
        assert!(third_party[beta..].contains("Second owner"));
        assert!(third_party.find("Second owner").unwrap() < third_party.find(MIT).unwrap());
        assert!(third_party.contains("Third owner"));
        notices.reverse();
        assert_eq!(render(&notices, APACHE, &own), output);
    }

    #[test]
    fn complex_notices_remain_verbatim_with_context_outside() {
        for text in [
            format!(
                "Mixed licenses\r\nCopyright First owner\r\n{MIT}\r\nAn exception.\r\nCopyright Second owner\r\n{APACHE}\r\n"
            ),
            format!(
                "License title\nVersion 2\n{MIT}\nPermission to reproduce verbatim.\nCopyright License author\n"
            ),
            APACHE.replace("original", "altered"),
            format!("Copyright Owner\n{MIT}\nAdditional condition."),
            format!("Licensed under another license.\nCopyright Owner\n{MIT}"),
            format!("Copyright Owner\n{MIT}\nBased on other code.\nAn additional condition."),
        ] {
            let mut entry = notice("component", &text);
            assert!(
                render(std::slice::from_ref(&entry), APACHE, "Application license.")
                    .contains(&text)
            );
            let context = "An unresolved upstream statement.";
            entry.note = Some(context.into());
            let output = render(&[entry], APACHE, "Application license.");
            assert!(output.contains(&text));
            assert!(output.find(context).unwrap() < output.find(&text).unwrap());
        }
    }

    #[test]
    fn copyright_years_preserve_gaps_and_joint_owners() {
        assert_eq!(
            compact_attributions([
                "Copyright (C) 2001-2002, First owner.\nAll rights reserved.",
                "Copyright (c) 2002, 2005 First owner\nAll rights reserved.",
                "Copyright (c) 2001 First owner and Second owner",
            ]),
            "Copyright (c) 2001-2002, 2005 First owner\nCopyright (c) 2001 First owner and Second owner\nAll rights reserved."
        );
    }

    #[test]
    fn inherited_isc_headers_keep_owners_and_source_paths() {
        let work = tempfile::tempdir().unwrap();
        fs::create_dir_all(work.path().join("src/nested")).unwrap();
        let terms = "Permission to use, copy, modify, and/or distribute this software.\nOR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.\n";
        fs::write(
            work.path().join("LICENSE"),
            format!("Other license.\nISC license\nCopyright Current owner\n{terms}"),
        )
        .unwrap();
        let sources = [
            ("src/first.rs", "Copyright 2001 First owner."),
            (
                "src/nested/second.rs",
                "Copyright 2002 Second owner.\nPortions Copyright (c) 2003, Joint owner.",
            ),
        ];
        for (path, owner) in sources {
            fs::write(work.path().join(path), format!(
                "// {}\n// SPDX-License-Identifier: ISC\n// Modifications copyright Current owner.\n// SPDX-License-Identifier: Apache-2.0 OR ISC\n\nfn implementation() {{}}\n",
                owner.replace('\n', "\n// ")
            )).unwrap();
        }
        fs::write(work.path().join("src/other.rs"), "// Copyright Other owner.\n// SPDX-License-Identifier: Apache-2.0 OR ISC\nfn implementation() {}\n// SPDX-License-Identifier: ISC\n").unwrap();
        let notices = isc_source_notices(work.path(), "component").unwrap();
        assert_eq!(notices.len(), sources.len());
        for (entry, (path, owner)) in notices.iter().zip(sources) {
            assert!(entry.source.starts_with(path));
            assert_eq!(entry.text, format!("{owner}\n\n{terms}"));
        }
    }

    #[test]
    fn collectors_preserve_notice_text_and_native_header_context() {
        let work = tempfile::tempdir().unwrap();
        fs::create_dir(work.path().join("libarchive")).unwrap();
        let text = format!(
            "Source context.\nCopyright First owner\n{MIT}\nThis product includes software developed by its contributors."
        );
        fs::write(work.path().join("NOTICE.txt"), &text).unwrap();
        fs::write(
            work.path().join("libarchive/example.c"),
            format!("/*-\n{text}\n*/"),
        )
        .unwrap();
        let mut notices = package_notices(work.path(), "component").unwrap();
        notices.extend(native_notices(work.path(), "libarchive", "component").unwrap());
        assert_eq!(notices.len(), 2);
        for entry in &notices {
            assert_eq!(entry.text, text);
        }
        assert_eq!(
            render(&notices, APACHE, "Application license.")
                .matches(&text)
                .count(),
            1
        );
    }
}
