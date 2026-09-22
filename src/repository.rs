//! Repository names and authorization. Parsing never performs network I/O.
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use url::Url;
pub mod job;
pub mod manager;
pub mod materialize;
pub mod selection;
pub mod transport;

const SYNTAX: &str = "Use https://host/owner/repo.git, ssh://git@host/owner/repo.git, or git@host:owner/repo.git, optionally followed by #branch";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Endpoint {
    Github,
    Https {
        host: String,
        port: u16,
    },
    Ssh {
        host: String,
        port: u16,
        user: String,
        absolute: bool,
    },
}

/// Transport aliases share this identity only where the hosting service defines them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    endpoint: Endpoint,
    components: Vec<String>,
}

impl Identity {
    pub fn storage_key(&self) -> String {
        blake3::hash(&serde_json::to_vec(self).expect("repository identity is serializable"))
            .to_hex()
            .to_string()
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.endpoint {
            Endpoint::Github => write!(f, "https://github.com/"),
            Endpoint::Https { host, port } => write!(f, "https://{host}:{port}/"),
            Endpoint::Ssh {
                host,
                port,
                user,
                absolute,
            } => {
                write!(
                    f,
                    "ssh://{}{}:{port}/{}",
                    if user.is_empty() {
                        String::new()
                    } else {
                        format!("{user}@")
                    },
                    host,
                    if *absolute { "" } else { "~/" }
                )
            }
        }?;
        write!(f, "{}", self.components.join("/"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub identity: Identity,
    /// The supplied transport, without a branch selector or credentials.
    pub transport: String,
    pub branch: Option<String>,
}

impl Repository {
    /// Local paths, including paths containing '#', return None unchanged.
    /// A malformed remote name is an error rather than a local-path fallback.
    pub fn parse(input: &str) -> Result<Option<Self>> {
        if !looks_remote(input) {
            return Ok(None);
        }
        let (transport, fragment) = input
            .split_once('#')
            .map_or((input, None), |(a, b)| (a, Some(b)));
        let branch = fragment.map(decode).transpose()?;
        if let Some(branch) = &branch {
            validate_branch(branch)?;
        }
        Ok(Some(Self {
            identity: parse_identity(transport, false)?,
            transport: transport.to_owned(),
            branch,
        }))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Rule(Identity);

impl Rule {
    pub fn parse(input: &str) -> Result<Self> {
        ensure!(
            !input.contains('#'),
            "Authorization rules cannot select a branch. {SYNTAX}"
        );
        Ok(Self(parse_identity(input, true)?))
    }

    pub fn matches(&self, repository: &Identity) -> bool {
        if self.0.endpoint != repository.endpoint {
            return false;
        }
        for (index, component) in self.0.components.iter().enumerate() {
            let Some(actual) = repository.components.get(index) else {
                return false;
            };
            if component == "**" {
                return true;
            }
            if component != "*" && component != actual {
                return false;
            }
        }
        self.0.components.len() == repository.components.len()
    }
}

pub fn authorize(rules: &[Rule], repository: &Repository) -> Result<()> {
    ensure!(
        rules.iter().any(|rule| rule.matches(&repository.identity)),
        "Repository is not authorized: {}",
        repository.identity
    );
    Ok(())
}

fn looks_remote(input: &str) -> bool {
    if input.starts_with('/') || input.starts_with("./") || input.starts_with("../") {
        return false;
    }
    if input.contains("://") || input.contains("::") {
        return true;
    }
    input
        .split_once(':')
        .is_some_and(|(host, _)| !host.contains('/'))
}

fn parse_identity(input: &str, rule: bool) -> Result<Identity> {
    ensure!(
        !input.is_empty()
            && !input
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '\\'),
        "Invalid repository identifier. {SYNTAX}"
    );
    let (mut endpoint, path) = if input.contains("://") {
        let url =
            Url::parse(input).map_err(|_| anyhow::anyhow!("Invalid repository URL. {SYNTAX}"))?;
        ensure!(
            matches!(url.scheme(), "https" | "ssh"),
            "Unsupported repository transport. {SYNTAX}"
        );
        ensure!(
            url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
            "Repository URLs cannot contain passwords, queries, or fragments here. {SYNTAX}"
        );
        let host = url
            .host_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Repository host is missing. {SYNTAX}"))?
            .to_ascii_lowercase();
        validate_host(&host)?;
        // Validate the original path; URL parsers normalize dot segments.
        let raw = input.split_once("://").unwrap().1;
        let path = raw.split_once('/').map(|(_, p)| p).unwrap_or("");
        let endpoint = if url.scheme() == "https" {
            ensure!(
                url.username().is_empty(),
                "HTTPS credentials are not accepted in repository URLs. {SYNTAX}"
            );
            Endpoint::Https {
                host,
                port: url.port().unwrap_or(443),
            }
        } else {
            validate_user(url.username())?;
            Endpoint::Ssh {
                host,
                port: url.port().unwrap_or(22),
                user: url.username().to_owned(),
                absolute: true,
            }
        };
        (endpoint, path)
    } else {
        let (authority, path) = input
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Invalid repository identifier. {SYNTAX}"))?;
        ensure!(
            !matches!(
                authority,
                "file" | "http" | "https" | "ssh" | "git" | "ftp" | "ext"
            ) && !path.contains('%'),
            "Unsupported or ambiguous repository transport. {SYNTAX}"
        );
        ensure!(
            !path.starts_with(':') && !authority.contains('/'),
            "Unsupported repository transport. {SYNTAX}"
        );
        let (user, host) = authority.split_once('@').unwrap_or(("", authority));
        validate_user(user)?;
        validate_host(host)?;
        (
            Endpoint::Ssh {
                host: host.to_ascii_lowercase(),
                port: 22,
                user: user.to_owned(),
                absolute: path.starts_with('/'),
            },
            path.strip_prefix('/').unwrap_or(path),
        )
    };
    let mut components = path.split('/').map(decode).collect::<Result<Vec<_>>>()?;
    ensure!(
        !components.is_empty(),
        "Repository path is missing. {SYNTAX}"
    );
    for (index, part) in components.iter().enumerate() {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.starts_with('-')
                && !part.chars().any(|c| c.is_control()
                    || c.is_whitespace()
                    || matches!(c, '/' | '\\' | ':' | '?' | '#' | '[' | ']' | '@')),
            "Invalid repository path. {SYNTAX}"
        );
        ensure!(
            !part.contains('*')
                || rule && (part == "*" || part == "**" && index + 1 == components.len()),
            "Rules allow whole-component '*' and terminal '**' only. {SYNTAX}"
        );
    }
    let github = match &endpoint {
        Endpoint::Https { host, port } => host == "github.com" && *port == 443,
        Endpoint::Ssh {
            host, port, user, ..
        } => host == "github.com" && *port == 22 && user == "git",
        Endpoint::Github => unreachable!(),
    };
    if github {
        ensure!(
            components.len() == 2
                || rule && components.last().is_some_and(|c| c == "**") && components.len() <= 2,
            "Use a GitHub repository URL, not a browsing URL. {SYNTAX}"
        );
        for component in &mut components {
            component.make_ascii_lowercase();
        }
        if let Some(last) = components.last_mut() {
            if let Some(without_suffix) = last.strip_suffix(".git") {
                *last = without_suffix.to_owned();
            }
            ensure!(!last.is_empty(), "Repository name is missing. {SYNTAX}");
        }
        endpoint = Endpoint::Github;
    }
    Ok(Identity {
        endpoint,
        components,
    })
}

fn validate_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b)),
        "Repository host must be literal. {SYNTAX}"
    );
    Ok(())
}

fn validate_user(user: &str) -> Result<()> {
    ensure!(
        !user.starts_with('-')
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "Invalid SSH username. {SYNTAX}"
    );
    Ok(())
}

fn decode(value: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = input.next().and_then(|b| (b as char).to_digit(16));
            let b = input.next().and_then(|b| (b as char).to_digit(16));
            let (Some(a), Some(b)) = (a, b) else {
                bail!("Invalid percent encoding. {SYNTAX}")
            };
            decoded.push((a * 16 + b) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| anyhow::anyhow!("Repository name is not UTF-8. {SYNTAX}"))
}

pub fn validate_branch(branch: &str) -> Result<()> {
    ensure!(
        !branch.is_empty()
            && branch != "@"
            && branch != "HEAD"
            && !branch.starts_with('-')
            && !branch.ends_with('.')
            && !branch.contains("..")
            && !branch.contains("@{")
            && !branch
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
            && branch
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock")),
        "Invalid branch selector; supply a branch name, such as #feature/search, rather than a revision expression"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo(value: &str) -> Repository {
        Repository::parse(value).unwrap().unwrap()
    }

    #[test]
    fn github_aliases_keep_the_requested_transport() {
        let inputs = [
            "https://GitHub.com/Owner/Repo.git",
            "ssh://git@github.com:22/owner/repo",
            "git@github.com:OWNER/REPO.git",
        ];
        let canonical = repo(inputs[0]).identity;
        for input in inputs {
            let parsed = repo(input);
            assert_eq!(parsed.identity, canonical);
            assert_eq!(parsed.transport, input);
        }
    }

    #[test]
    fn custom_endpoints_remain_distinct() {
        let inputs = [
            "https://example.com/Team/Repo",
            "https://example.com/team/Repo",
            "https://example.com/Team/Repo.git",
            "ssh://git@example.com/Team/Repo",
            "ssh://other@example.com/Team/Repo",
            "ssh://git@example.com:2222/Team/Repo",
            "git@example.com:Team/Repo",
        ];
        let identities: std::collections::HashSet<_> = inputs.map(|s| repo(s).identity).into();
        assert_eq!(identities.len(), inputs.len());
        assert_eq!(
            repo("git@example.com:/Team/Repo").identity,
            repo("ssh://git@example.com:22/Team/Repo").identity
        );
        assert_eq!(
            repo("https://EXAMPLE.com:443/Team/Repo").identity,
            repo(inputs[0]).identity
        );
    }

    #[test]
    fn fragments_are_decoded_once_and_preserve_case() {
        let r = repo("https://github.com/o/r#Feature%2Fsearch%2520literal");
        assert_eq!(r.branch.as_deref(), Some("Feature/search%20literal"));
        assert!(!r.transport.contains('#'));
        for path in [
            "/tmp/project#branch",
            "./project#branch",
            "project#branch",
            "../project#branch",
        ] {
            assert!(Repository::parse(path).unwrap().is_none());
        }
    }

    #[test]
    fn invalid_inputs_never_fall_back_to_local_paths() {
        for input in [
            "https://github.com/o/r/tree/main",
            "https://github.com/o/r?token=secret",
            "https://user:secret@github.com/o/r",
            "https://github.com/o/../r",
            "https://github.com/o/%2e%2e",
            "https://github.com/o/r%2fs",
            "https://github.com/o/r#main~1",
            "https://github.com/o/r#branch%",
            "https://github.com/o/r#",
            "file:///tmp/repo",
            "ext::command",
            "git://example.com/repo",
            "ssh://git@example.com/repo#x.lock",
            "https://github.com/o/r#x%00y",
        ] {
            assert!(Repository::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn rules_match_components_and_canonical_aliases() {
        let rules = [
            Rule::parse("https://github.com/owner/*").unwrap(),
            Rule::parse("https://example.com/team/**").unwrap(),
            Rule::parse("ssh://git@private.example/**").unwrap(),
        ];
        for input in [
            "git@github.com:owner/repo.git#Feature/One",
            "https://example.com/team/repo",
            "https://example.com/team/group/repo",
            "ssh://git@private.example/a/b",
        ] {
            authorize(&rules, &repo(input)).unwrap();
        }
        for input in [
            "https://github.com/owners/repo",
            "https://example.com/teams/repo",
            "https://example.com/team",
            "https://example.com/team/../outside",
            "ssh://other@private.example/a",
            "git@private.example:a",
            "https://private.example/a",
        ] {
            assert!(
                Repository::parse(input)
                    .and_then(|r| authorize(&rules, &r.unwrap()))
                    .is_err(),
                "{input}"
            );
        }
        let exact = Rule::parse("https://github.com/owner/repo.git").unwrap();
        assert!(exact.matches(&repo("git@github.com:owner/repo#main").identity));
        assert!(!exact.matches(&repo("https://github.com/owner/repository").identity));
        for input in [
            "https://*.example.com/team/*",
            "https://example.com/team/pre*",
            "https://example.com/**/repo",
            "https://example.com/team/*#main",
        ] {
            assert!(Rule::parse(input).is_err(), "{input}");
        }
    }
}
