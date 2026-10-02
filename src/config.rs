use crate::{
    repository::Rule,
    unity::{Platform, ReleaseBranch},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, time::Duration};
pub(crate) const DEFAULT_REPO_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    #[default]
    Local,
    Remote,
    Hybrid,
}

#[derive(Args, Debug)]
pub struct Options {
    #[arg(long, global = true, default_value = "local")]
    pub mode: Mode,
    /// HTTP MCP endpoint used by hybrid mode.
    #[arg(long, global = true)]
    pub upstream: Option<String>,
    /// File containing the upstream bearer token.
    #[arg(long, global = true)]
    pub upstream_token_file: Option<PathBuf>,
    #[arg(long, global = true, alias = "cache", default_value = "/tmp/sigla")]
    pub cache_dir: PathBuf,
    /// Permitted local roots. Remote mode has no implicit local roots.
    #[arg(long, global = true)]
    pub root: Vec<PathBuf>,
    #[arg(long, global = true, default_value = "UNITY_EDITOR_LINUX")]
    pub unity_platform: Platform,
    /// Local Unity Hub editors directory, containing version subdirectories.
    #[arg(long, global = true)]
    pub unity_editors: Option<PathBuf>,
    /// Allow anonymous access to matching public repositories.
    #[arg(long, global = true)]
    pub allow_repo: Vec<String>,
    /// Allow authenticated access to matching repositories, including private repositories.
    #[arg(long, global = true)]
    pub allow_repo_private: Vec<String>,
    /// Fixed remote refresh interval; when omitted, polling slows with idle time.
    #[arg(long, global = true, value_parser = duration)]
    pub refresh_interval: Option<Duration>,
    #[arg(long, global = true, value_parser = duration)]
    pub repo_ttl: Option<Duration>,
    #[arg(long, global = true, value_parser = duration)]
    pub branch_ttl: Option<Duration>,
    #[arg(long, global = true)]
    pub unity_version: Vec<ReleaseBranch>,
    #[arg(long, global = true)]
    pub remote_include: Vec<String>,
    #[arg(long, global = true)]
    pub remote_exclude: Vec<String>,
}

pub struct RemoteOptions {
    pub rules: Vec<Rule>,
    pub refresh_interval: Option<Duration>,
    pub repo_ttl: Duration,
    pub branch_ttl: Duration,
    pub unity_versions: Vec<ReleaseBranch>,
    pub selection: crate::repository::selection::Selection,
}

impl RemoteOptions {
    pub fn refresh_delay(&self, idle: Duration) -> Duration {
        self.refresh_interval.unwrap_or_else(|| {
            let hours = idle.as_secs_f64() / 3600.0;
            Duration::from_secs_f64(15.0 + 1785.0 * -(-hours / 28.74).exp_m1())
        })
    }

    pub fn maintenance_interval(&self) -> Duration {
        self.refresh_interval
            .map_or(Duration::from_secs(1), |fixed| {
                fixed.min(Duration::from_secs(60))
            })
    }
}

impl Options {
    pub fn validate(&self) -> Result<Option<RemoteOptions>> {
        if self.mode == Mode::Hybrid {
            crate::upstream::validate_endpoint(
                self.upstream
                    .as_deref()
                    .context("Hybrid mode requires --upstream")?,
            )?;
        } else {
            ensure!(
                self.upstream.is_none() && self.upstream_token_file.is_none(),
                "--upstream and --upstream-token-file require --mode hybrid"
            );
        }
        if self.mode != Mode::Remote {
            ensure!(
                self.allow_repo.is_empty()
                    && self.allow_repo_private.is_empty()
                    && self.refresh_interval.is_none()
                    && self.repo_ttl.is_none()
                    && self.branch_ttl.is_none()
                    && self.unity_version.is_empty()
                    && self.remote_include.is_empty()
                    && self.remote_exclude.is_empty(),
                "--allow-repo, --allow-repo-private, --refresh-interval, --repo-ttl, --branch-ttl, --unity-version, --remote-include, and --remote-exclude require --mode remote"
            );
            return Ok(None);
        }
        ensure!(
            !self.allow_repo.is_empty() || !self.allow_repo_private.is_empty(),
            "Remote mode requires --allow-repo or --allow-repo-private"
        );
        Ok(Some(RemoteOptions {
            rules: self
                .allow_repo
                .iter()
                .map(|r| Rule::parse(r))
                .chain(
                    self.allow_repo_private
                        .iter()
                        .map(|r| Rule::parse_private(r)),
                )
                .collect::<Result<_>>()?,
            refresh_interval: self.refresh_interval,
            repo_ttl: self.repo_ttl.unwrap_or(DEFAULT_REPO_TTL),
            branch_ttl: self.branch_ttl.unwrap_or(Duration::from_secs(24 * 60 * 60)),
            unity_versions: self.unity_version.clone(),
            selection: crate::repository::selection::Selection::new(
                &self.remote_include,
                &self.remote_exclude,
            )?,
        }))
    }

    pub fn local_roots(&self) -> Vec<PathBuf> {
        if self.mode != Mode::Remote && self.root.is_empty() {
            vec![PathBuf::from("/")]
        } else {
            self.root.clone()
        }
    }
}

fn duration(value: &str) -> Result<Duration> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .context("Duration requires a unit, such as 5m or 7d")?;
    let count: u64 = value[..split].parse().context("Invalid duration")?;
    let unit = match &value[split..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("Duration unit must be ms, s, m, h, or d"),
    };
    ensure!(count > 0, "Duration must be positive");
    Ok(Duration::from_millis(
        count.checked_mul(unit).context("Duration is too large")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        options: Options,
    }
    #[test]
    fn refresh_curve_is_monotone_bounded_and_exponential() {
        let options = Cli::try_parse_from([
            "sigla",
            "--mode",
            "remote",
            "--allow-repo",
            "https://github.com/owner/*",
        ])
        .unwrap()
        .options
        .validate()
        .unwrap()
        .unwrap();
        assert!(options.refresh_interval.is_none());
        let active = options.refresh_delay(Duration::ZERO);
        let maximum = options.refresh_delay(Duration::MAX);
        assert!(active > Duration::ZERO && active < maximum);
        assert!(options.maintenance_interval() < active);
        let mut previous = active;
        for minutes in [5, 30, 60, 120, 300, 720, 1440, 2880, 4320, 10080] {
            let next = options.refresh_delay(Duration::from_secs(minutes * 60));
            assert!(next > previous && next < maximum);
            previous = next;
        }
        // Equal idle-time increments shrink the remaining gap by the same ratio.
        let gap = |hours: u64| {
            (maximum - options.refresh_delay(Duration::from_secs(hours * 3600))).as_secs_f64()
        };
        assert!((gap(1) / gap(0) - gap(2) / gap(1)).abs() < 1e-8);
    }

    #[test]
    fn explicit_refresh_interval_ignores_recency() {
        let options = Cli::try_parse_from([
            "sigla",
            "--mode",
            "remote",
            "--allow-repo",
            "https://github.com/owner/*",
            "--refresh-interval",
            "23s",
        ])
        .unwrap()
        .options
        .validate()
        .unwrap()
        .unwrap();
        let fixed = options.refresh_interval.unwrap();
        for idle in [Duration::ZERO, Duration::from_secs(86400), Duration::MAX] {
            assert_eq!(options.refresh_delay(idle), fixed);
        }
        assert!(options.maintenance_interval() <= fixed);
    }

    #[test]
    fn mode_controls_roots_and_remote_options() {
        let local = Cli::try_parse_from(["sigla"]).unwrap().options;
        assert!(local.validate().unwrap().is_none());
        assert!(!local.local_roots().is_empty());
        let remote = Cli::try_parse_from([
            "sigla",
            "--mode",
            "remote",
            "--allow-repo",
            "https://github.com/owner/*",
        ])
        .unwrap()
        .options;
        assert!(remote.validate().unwrap().is_some());
        assert!(remote.local_roots().is_empty());
        let private = Cli::try_parse_from([
            "sigla",
            "--mode",
            "remote",
            "--allow-repo-private",
            "https://github.com/owner/private",
        ])
        .unwrap()
        .options;
        assert!(private.validate().unwrap().is_some());
        let local_private = Cli::try_parse_from([
            "sigla",
            "--allow-repo-private",
            "https://github.com/owner/private",
        ])
        .unwrap()
        .options;
        assert!(local_private.validate().is_err());
        for flag in ["--refresh-interval", "--repo-ttl", "--branch-ttl"] {
            let local = Cli::try_parse_from(["sigla", flag, "5m"]).unwrap().options;
            assert!(local.validate().is_err());
        }
        for flag in [
            "--csharp-project-mode",
            "--framework",
            "--configuration",
            "--restore",
            "--platform",
            "--unity",
        ] {
            assert!(Cli::try_parse_from(["sigla", flag, "value"]).is_err());
        }
    }
    #[test]
    fn hybrid_requires_endpoint_and_keeps_local_roots() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().options;
        let hybrid = parse(&[
            "sigla",
            "--mode",
            "hybrid",
            "--upstream",
            "https://example.com/mcp",
        ]);
        assert!(hybrid.validate().unwrap().is_none());
        assert_eq!(hybrid.local_roots(), parse(&["sigla"]).local_roots());
        for args in [
            vec!["sigla", "--mode", "hybrid"],
            vec!["sigla", "--upstream", "https://example.com/mcp"],
            vec!["sigla", "--upstream-token-file", "token"],
            vec![
                "sigla",
                "--mode",
                "hybrid",
                "--upstream",
                "http://example.com/mcp",
            ],
            vec![
                "sigla",
                "--mode",
                "hybrid",
                "--upstream",
                "https://user:secret@example.com/mcp",
            ],
            vec![
                "sigla",
                "--mode",
                "hybrid",
                "--upstream",
                "https://example.com/mcp",
                "--allow-repo",
                "https://github.com/owner/*",
            ],
        ] {
            assert!(parse(&args).validate().is_err(), "{args:?}");
        }
        for endpoint in [
            "http://127.0.0.1:7331/mcp",
            "http://[::1]:7331/mcp",
            "http://localhost/mcp",
        ] {
            assert!(
                parse(&["sigla", "--mode", "hybrid", "--upstream", endpoint])
                    .validate()
                    .is_ok()
            );
        }
    }
    #[test]
    fn durations_require_valid_units_and_cannot_overflow() {
        assert!(duration("5m").unwrap() < duration("24h").unwrap());
        for invalid in ["5", "0s", "-1h", "1year", "999999999999999999d"] {
            assert!(duration(invalid).is_err());
        }
    }
}
