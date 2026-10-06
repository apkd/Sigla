//! Noninteractive authentication settings shared by repository subprocesses.
use std::process::Command;

pub(super) fn noninteractive(command: &mut Command) -> &mut Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "false")
        .env("SSH_ASKPASS", "false")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("DISPLAY")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

    fn executable(name: &str) -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join(name))
            .find(|path| {
                path.is_file() && path.metadata().unwrap().permissions().mode() & 0o111 != 0
            })
            .unwrap_or_else(|| panic!("Missing test dependency: {name}"))
    }

    fn askpass_fixture(command: &mut Command, variable: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let shell = executable("sh");
        command.env_clear();
        noninteractive(command);
        let helper = command
            .get_envs()
            .find(|(key, _)| *key == variable)
            .unwrap()
            .1
            .unwrap()
            .to_owned();
        let path = directory
            .path()
            .join(std::path::Path::new(&helper).file_name().unwrap());
        fs::write(
            &path,
            format!(
                "#!{}\nprintf invoked > \"$ASKPASS_MARKER\"\nexit 1\n",
                shell.display()
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env("PATH", directory.path())
            .env("ASKPASS_MARKER", directory.path().join("invoked"));
        directory
    }

    #[test]
    fn askpass_fails_without_input_or_output() {
        let mut settings = Command::new("git");
        noninteractive(&mut settings);
        let program = settings
            .get_envs()
            .find(|(key, _)| *key == "GIT_ASKPASS")
            .unwrap()
            .1
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(std::path::Path::new(program).file_name().unwrap());
        std::os::unix::fs::symlink(executable(program.to_str().unwrap()), &path).unwrap();
        let output = crate::process::capture(
            Command::new(program)
                .env_clear()
                .env("PATH", directory.path())
                .arg("Password:"),
            Duration::from_secs(5),
            None,
            None,
        )
        .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }

    #[test]
    fn missing_git_credentials_invoke_askpass_and_fail_promptly() {
        let mut command = Command::new(executable("git"));
        let fixture = askpass_fixture(&mut command, "GIT_ASKPASS");
        command
            .current_dir(fixture.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "credential.helper=", "credential", "fill"]);
        let output = crate::process::capture(
            &mut command,
            Duration::from_secs(5),
            Some(b"url=https://example.invalid/repo.git\n\n".to_vec()),
            None,
        )
        .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(fixture.path().join("invoked").is_file());
    }

    #[test]
    fn openssh_resolves_askpass_from_path_and_fails_promptly() {
        let keygen = executable("ssh-keygen");
        let mut command = Command::new(&keygen);
        let fixture = askpass_fixture(&mut command, "SSH_ASKPASS");
        let key = fixture.path().join("key");
        crate::process::run(
            Command::new(&keygen)
                .args(["-q", "-t", "ed25519", "-N", "test-passphrase", "-f"])
                .arg(&key),
            Duration::from_secs(5),
        )
        .unwrap();
        command
            .args(["-y", "-f"])
            .arg(&key)
            .env("SSH_ASKPASS_REQUIRE", "force");
        let output =
            crate::process::capture(&mut command, Duration::from_secs(5), None, None).unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(fixture.path().join("invoked").is_file());
    }
}
