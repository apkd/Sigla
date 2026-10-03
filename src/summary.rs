//! Repository facts travel separately from presentation so proxies can enforce
//! their own MCP session lifetime without interpreting rendered Markdown.
use crate::{model::Language, render::inline, workspace::Manifest};
use rmcp::model::{CallToolResult, ContentBlock, MetaObject, TextContent};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const CONTEXT: &str = "sigla/repository";
const PREAMBLE: &str = "sigla/repository-summary";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub key: String,
    pub table: String,
}

fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn list(values: &[String]) -> String {
    let mut result = values
        .iter()
        .take(6)
        .map(|s| inline(&cell(s)))
        .collect::<Vec<_>>()
        .join(", ");
    if values.len() > 6 {
        result.push_str(&format!(", … (+{})", values.len() - 6));
    }
    result
}

impl Summary {
    pub fn build(
        identity: &crate::repository::Identity,
        branch: &str,
        revision: &str,
        tracked: usize,
        root: &Path,
        manifest: &Manifest,
    ) -> Self {
        let files: BTreeSet<_> = manifest
            .files
            .values()
            .filter(|f| !f.metadata)
            .filter_map(|f| f.path.strip_prefix(root).ok())
            .collect();
        let mut projects: Vec<_> = manifest
            .projects
            .iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| {
                (
                    p.origin.as_ref().is_some_and(|p| !p.starts_with(root)),
                    p.name.clone(),
                )
            })
            .collect();
        projects.sort();
        let mut names = BTreeSet::new();
        let projects: Vec<_> = projects
            .into_iter()
            .map(|(_, name)| name)
            .filter(|n| names.insert(n.clone()))
            .collect();
        let mut docs = BTreeSet::new();
        for path in &files {
            let text = path.to_string_lossy().replace('\\', "/");
            let lower = text.to_ascii_lowercase();
            if !lower.contains('/') {
                let rank = if matches!(lower.as_str(), "agents.md" | "claude.md") {
                    Some(0)
                } else if lower.starts_with("readme") {
                    Some(1)
                } else if lower.starts_with("contributing") {
                    Some(2)
                } else {
                    None
                };
                if let Some(rank) = rank {
                    docs.insert((rank, text));
                }
            } else if lower.starts_with(".github/instructions/") {
                docs.insert((
                    3,
                    text.split('/').take(2).collect::<Vec<_>>().join("/") + "/",
                ));
            } else if matches!(
                lower.split('/').next(),
                Some("doc" | "docs" | "documentation")
            ) {
                docs.insert((3, text.split('/').next().unwrap().to_owned() + "/"));
            }
        }
        let mut stack = BTreeSet::new();
        for file in manifest
            .files
            .values()
            .filter(|f| !f.metadata && f.path.starts_with(root))
        {
            match file.language {
                Language::CSharp => {
                    stack.insert("C#".to_owned());
                }
                Language::Rust => {
                    stack.insert("Rust".to_owned());
                }
                Language::C | Language::Cpp | Language::Header => {
                    stack.insert("C/C++".to_owned());
                }
                Language::Hlsl | Language::Glsl | Language::ShaderLab => {
                    stack.insert("Shaders".to_owned());
                }
                _ => (),
            }
        }
        for project in &manifest.projects {
            if let Some(version) = project.compiler_options.get("UnityVersion") {
                stack.insert(format!("Unity {version} (selected editor)"));
            }
            if let Some(framework) = project.compiler_options.get("TargetFramework")
                && !framework.is_empty()
            {
                stack.insert(framework.clone());
            }
        }
        let identity_text = identity.to_string();
        let repo = identity_text
            .strip_prefix("https://github.com/")
            .unwrap_or(&identity_text);
        let mut columns = vec!["Repository", "Revision", "Projects", "Files"];
        let mut values = vec![
            inline(&cell(repo)),
            inline(&cell(&format!(
                "{branch} @ {}",
                &revision[..revision.len().min(8)]
            ))),
            list(&projects),
            format!("{tracked} tracked · {} indexed", files.len()),
        ];
        if !stack.is_empty() {
            columns.push("Stack");
            values.push(cell(&stack.into_iter().collect::<Vec<_>>().join(" · ")));
        }
        if !docs.is_empty() {
            columns.push("Documentation");
            values.push(list(&docs.into_iter().map(|(_, p)| p).collect::<Vec<_>>()));
        }
        if !manifest.diagnostics.is_empty() {
            columns.push("Coverage");
            values.push(format!(
                "Incomplete: {} indexing diagnostics",
                manifest.diagnostics.len()
            ));
        }
        Self {
            key: format!("{}#{branch}", identity.storage_key()),
            table: format!(
                "| {} |\n| {} |\n| {} |",
                columns.join(" | "),
                vec!["---"; columns.len()].join(" | "),
                values.join(" | ")
            ),
        }
    }

    pub fn attach(self, result: &mut CallToolResult) {
        result
            .meta
            .get_or_insert_with(MetaObject::default)
            .0
            .insert(CONTEXT.into(), serde_json::to_value(self).unwrap());
    }
}

#[derive(Default)]
pub(crate) struct Session(Mutex<BTreeSet<String>>);

impl Session {
    pub fn present(&self, result: CallToolResult) -> CallToolResult {
        present(result, |key| self.0.lock().unwrap().insert(key))
    }
}

const IDLE_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(crate) struct Stateless(Mutex<BTreeMap<String, Instant>>);

impl Stateless {
    pub fn present(&self, result: CallToolResult) -> CallToolResult {
        self.present_with_clock(result, Instant::now)
    }

    fn present_with_clock(
        &self,
        result: CallToolResult,
        clock: impl FnOnce() -> Instant,
    ) -> CallToolResult {
        present(result, |key| {
            let mut entries = self.0.lock().unwrap();
            let now = clock();
            entries.retain(|_, last| now.duration_since(*last) < IDLE_WINDOW);
            entries.insert(key, now).is_none()
        })
    }
}

fn present(mut result: CallToolResult, first: impl FnOnce(String) -> bool) -> CallToolResult {
    if result.is_error == Some(true) {
        return result;
    }
    let summary = result
        .meta
        .as_ref()
        .and_then(|m| m.0.get(CONTEXT))
        .and_then(|v| serde_json::from_value::<Summary>(v.clone()).ok());
    let Some(summary) = summary else {
        return result;
    };
    result.content.retain(|block| {
        !block.as_text().is_some_and(|text| {
            text.meta
                .as_ref()
                .is_some_and(|m| m.0.get(PREAMBLE) == Some(&serde_json::Value::Bool(true)))
        })
    });
    if first(summary.key) {
        let meta = MetaObject(serde_json::Map::from_iter([(PREAMBLE.into(), true.into())]));
        result.content.insert(
            0,
            ContentBlock::Text(TextContent::new(summary.table).with_meta(meta)),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(key: &str, table: &str) -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text("result")]);
        Summary {
            key: key.into(),
            table: table.into(),
        }
        .attach(&mut result);
        result
    }

    #[test]
    fn stateless_window_slides_and_failures_do_not_extend_it() {
        let state = Stateless::default();
        let start = Instant::now();
        let step = IDLE_WINDOW / 2;
        let call = |key, time| state.present_with_clock(response(key, "summary"), || time);
        assert_eq!(call("repo#refs/heads/main", start).content.len(), 2);
        assert_eq!(call("repo#refs/heads/main", start + step).content.len(), 1);
        assert_eq!(
            call("repo#refs/heads/main", start + step * 2).content.len(),
            1
        );
        for key in [
            "repo#refs/tags/main",
            "repo#commit",
            "other#refs/heads/main",
        ] {
            assert_eq!(call(key, start + step * 2).content.len(), 2);
        }
        let mut failed = response("repo#refs/heads/main", "summary");
        failed.is_error = Some(true);
        state.present_with_clock(failed, || start + step * 3);
        assert_eq!(
            call("repo#refs/heads/main", start + step * 4).content.len(),
            2
        );
    }

    #[test]
    fn stateless_concurrent_results_strip_upstream_headers_and_emit_once() {
        let state = Stateless::default();
        let upstream = Session::default().present(response("repo", "summary"));
        let count = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|_| {
                    let upstream = &upstream;
                    let state = &state;
                    scope.spawn(move || state.present(upstream.clone()).content.len() - 1)
                })
                .collect();
            tasks.into_iter().map(|t| t.join().unwrap()).sum::<usize>()
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn session_isolates_projects_branches_and_ignores_revision_changes() {
        let session = Session::default();
        let first = session.present(response("repo#main", "old revision"));
        assert_eq!(first.content.len(), 2);
        assert_eq!(
            session
                .present(response("repo#main", "new revision"))
                .content
                .len(),
            1
        );
        assert_eq!(
            session
                .present(response("repo#feature", "other branch"))
                .content
                .len(),
            2
        );
        assert_eq!(
            Session::default()
                .present(response("repo#main", "fresh session"))
                .content
                .len(),
            2
        );
        let proxy = Session::default();
        assert_eq!(proxy.present(first.clone()).content.len(), 2);
        assert_eq!(proxy.present(first).content.len(), 1);
    }

    #[test]
    fn failures_do_not_consume_summary_and_concurrent_results_emit_once() {
        let session = Session::default();
        let mut failed = response("repo", "summary");
        failed.is_error = Some(true);
        assert_eq!(session.present(failed).content.len(), 1);
        let count = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| session.present(response("repo", "summary")).content.len() - 1)
                })
                .collect();
            tasks.into_iter().map(|t| t.join().unwrap()).sum::<usize>()
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn table_uses_index_facts_and_canonical_repository_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("docs")).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "Instructions").unwrap();
        std::fs::write(root.path().join("docs/guide.md"), "Guide").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut workspace = crate::workspace::Workspace::open(
            root.path().into(),
            cache.path(),
            crate::discovery::Policy::new(vec![root.path().into()]).unwrap(),
            &cache.path().join("analysis"),
            None,
            Default::default(),
        )
        .unwrap();
        workspace.refresh().unwrap();
        let repo = crate::repository::Repository::parse("https://github.com/owner/repo")
            .unwrap()
            .unwrap();
        let alias = crate::repository::Repository::parse("git@github.com:owner/repo.git")
            .unwrap()
            .unwrap();
        let summary = Summary::build(
            &repo.identity,
            "main",
            "1234567890abcdef",
            7,
            root.path(),
            &workspace.manifest,
        );
        let other = Summary::build(
            &alias.identity,
            "main",
            "abcdef1234567890",
            7,
            root.path(),
            &workspace.manifest,
        );
        assert_eq!(summary.key, other.key);
        assert!(
            summary.table.contains("7 tracked · 2 indexed"),
            "{}",
            summary.table
        );
        assert!(summary.table.contains("AGENTS.md") && summary.table.contains("docs/"));
        assert!(!summary.table.contains("Coverage") && !summary.table.contains("Stack"));
        assert_eq!(summary.table.lines().count(), 3);
        let values = (0..10).map(|i| format!("project{i}")).collect::<Vec<_>>();
        let rendered = list(&values);
        assert!(
            rendered.contains("project0")
                && rendered.contains("(+4)")
                && !rendered.contains("project9")
        );
    }
}
