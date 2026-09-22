use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReleaseBranch {
    pub major: u32,
    pub minor: u32,
}

impl FromStr for ReleaseBranch {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (major, minor) = value
            .split_once('.')
            .context("Unity release branch must have the form major.minor")?;
        ensure!(
            !major.is_empty()
                && !minor.is_empty()
                && major
                    .bytes()
                    .chain(minor.bytes())
                    .all(|c| c.is_ascii_digit()),
            "Unity release branch must have the form major.minor, not a patch version or latest"
        );
        Ok(Self {
            major: major.parse()?,
            minor: minor.parse()?,
        })
    }
}
impl fmt::Display for ReleaseBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnityVersion {
    pub branch: ReleaseBranch,
    pub patch: u32,
    channel: u8,
    pub revision: u32,
}

impl UnityVersion {
    pub fn stable(self) -> bool {
        self.channel >= 2
    }
}
impl FromStr for UnityVersion {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (prefix, revision) = value
            .split_once(['a', 'b', 'f', 'p'])
            .context("Invalid Unity editor version")?;
        let (branch, patch) = prefix
            .rsplit_once('.')
            .context("Invalid Unity editor version")?;
        let channel = match value.as_bytes()[prefix.len()] {
            b'a' => 0,
            b'b' => 1,
            b'f' => 2,
            b'p' => 3,
            _ => unreachable!(),
        };
        ensure!(
            !patch.is_empty()
                && !revision.is_empty()
                && patch
                    .bytes()
                    .chain(revision.bytes())
                    .all(|c| c.is_ascii_digit()),
            "Invalid Unity editor version"
        );
        Ok(Self {
            branch: branch.parse()?,
            patch: patch.parse()?,
            channel,
            revision: revision.parse()?,
        })
    }
}
impl fmt::Display for UnityVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}{}{}",
            self.branch,
            self.patch,
            ['a', 'b', 'f', 'p'][self.channel as usize],
            self.revision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_compare_numerically_and_branches_reject_overrides() {
        assert!("6000.3.10f1".parse::<UnityVersion>().unwrap() > "6000.3.9f9".parse().unwrap());
        assert!("2022.3.18f1".parse::<UnityVersion>().unwrap().stable());
        assert!(!"6000.3.0b9".parse::<UnityVersion>().unwrap().stable());
        for invalid in [
            "latest",
            "2022",
            "2022.3.1",
            "2022.3.18f1",
            "2022.-3",
            " 2022.3",
        ] {
            assert!(invalid.parse::<ReleaseBranch>().is_err());
        }
    }
}
