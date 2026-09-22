use crate::{
    repository::Rule,
    unity::{Platform, ReleaseBranch},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    #[default]
    Local,
    Remote,
}

#[derive(Args, Debug)]
pub struct Options {
    #[arg(long, global = true, default_value = "local")]
    pub mode: Mode,
    #[arg(long, global = true, alias = "cache", default_value = "/tmp/sigla")]
    pub cache_dir: PathBuf,
    /// Permitted local roots. Remote mode has no implicit local roots.
    #[arg(long, global = true)]
    pub root: Vec<PathBuf>,
    #[arg(long, global = true, default_value = "UNITY_EDITOR_LINUX")]
    pub unity_platform: Platform,
    #[arg(long, global = true)]
    pub allow_repo: Vec<String>,
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
    pub refresh_interval: Duration,
    pub repo_ttl: Duration,
    pub branch_ttl: Duration,
    pub unity_versions: Vec<ReleaseBranch>,
    pub selection: crate::repository::selection::Selection,
}

impl Options {
    pub fn validate(&self) -> Result<Option<RemoteOptions>> {
        if self.mode == Mode::Local {
            ensure!(
                self.allow_repo.is_empty()
                    && self.refresh_interval.is_none()
                    && self.repo_ttl.is_none()
                    && self.branch_ttl.is_none()
                    && self.unity_version.is_empty()
                    && self.remote_include.is_empty()
                    && self.remote_exclude.is_empty(),
                "--allow-repo, --refresh-interval, --repo-ttl, --branch-ttl, --unity-version, --remote-include, and --remote-exclude require --mode remote"
            );
            return Ok(None);
        }
        ensure!(
            !self.allow_repo.is_empty(),
            "Remote mode requires at least one --allow-repo rule"
        );
        Ok(Some(RemoteOptions {
            rules: self
                .allow_repo
                .iter()
                .map(|r| Rule::parse(r))
                .collect::<Result<_>>()?,
            refresh_interval: self.refresh_interval.unwrap_or(Duration::from_secs(5 * 60)),
            repo_ttl: self
                .repo_ttl
                .unwrap_or(Duration::from_secs(7 * 24 * 60 * 60)),
            branch_ttl: self.branch_ttl.unwrap_or(Duration::from_secs(24 * 60 * 60)),
            unity_versions: self.unity_version.clone(),
            selection: crate::repository::selection::Selection::new(
                &self.remote_include,
                &self.remote_exclude,
            )?,
        }))
    }

    pub fn local_roots(&self) -> Vec<PathBuf> {
        if self.mode == Mode::Local && self.root.is_empty() {
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
    fn durations_require_valid_units_and_cannot_overflow() {
        assert!(duration("5m").unwrap() < duration("24h").unwrap());
        for invalid in ["5", "0s", "-1h", "1year", "999999999999999999d"] {
            assert!(duration(invalid).is_err());
        }
    }
}
