//! Migration must not overwrite a completed cooperating user-config mutation.

#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Stdio};
use std::thread::JoinHandle;

use fs2::FileExt;
use worktrunk::config::UserConfig;

use crate::common::{test_tempdir, wait_for, wt_command};

struct MigrationProcess {
    child: Option<Child>,
    mutation: Option<JoinHandle<()>>,
}

impl Drop for MigrationProcess {
    fn drop(&mut self) {
        // Release the child's config lock before joining a queued mutation.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mutation) = self.mutation.take() {
            let _ = mutation.join();
        }
    }
}

fn child_has_file_open(pid: u32, path: &Path) -> bool {
    let expected = fs::metadata(path).unwrap();
    let Ok(mut entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries.any(|entry| {
        // Unrelated descriptors can close during enumeration. The held
        // approvals lock cannot disappear before this test releases it.
        entry
            .and_then(|entry| fs::metadata(entry.path()))
            .is_ok_and(|actual| actual.dev() == expected.dev() && actual.ino() == expected.ino())
    })
}

fn mutate_config(path: &Path) {
    UserConfig::default()
        .set_skip_shell_integration_prompt(path)
        .unwrap();
}

#[test]
fn test_config_update_preserves_a_cooperating_mutation() {
    let directory = test_tempdir();
    let config = directory.path().join("config.toml");
    fs::write(
        &config,
        "[projects.fixture]\napproved-commands = [\"echo fixture\"]\n",
    )
    .unwrap();
    let approvals_lock_path = directory.path().join("approvals.toml.lock");
    let approvals_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&approvals_lock_path)
        .unwrap();
    FileExt::lock_exclusive(&approvals_lock).unwrap();

    let child = wt_command()
        .arg("--config")
        .arg(&config)
        .args(["config", "update", "--yes"])
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut migration = MigrationProcess {
        child: Some(child),
        mutation: None,
    };
    let pid = migration.child.as_ref().unwrap().id();
    wait_for(
        "migration to open its approvals lock after rechecking config",
        || {
            assert!(
                migration
                    .child
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none()
            );
            child_has_file_open(pid, &approvals_lock_path)
        },
    );

    let config_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.path().join("config.toml.lock"))
        .unwrap();
    let migration_holds_config_lock = match FileExt::try_lock_exclusive(&config_lock) {
        Ok(()) => {
            FileExt::unlock(&config_lock).unwrap();
            false
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(error) => panic!("probe config lock: {error}"),
    };
    if migration_holds_config_lock {
        // A serialized implementation legitimately queues this mutation.
        let path = config.clone();
        migration.mutation = Some(std::thread::spawn(move || mutate_config(&path)));
        println!("migration holds the config lock; ordinary mutation scheduled");
    } else {
        // With no config lock, complete a real public mutation before the
        // migration can proceed. This is the lost-update interleaving.
        mutate_config(&config);
        let current: UserConfig = toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert!(current.skip_shell_integration_prompt);
        println!("ordinary mutation completed while migration waited for approvals");
    }
    FileExt::unlock(&approvals_lock).unwrap();
    wait_for("migration to finish", || {
        migration
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some()
    });
    let output = migration.child.take().unwrap().wait_with_output().unwrap();
    if let Some(mutation) = migration.mutation.take() {
        mutation.join().unwrap();
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contents = fs::read_to_string(&config).unwrap();
    let updated: UserConfig = toml::from_str(&contents).unwrap();
    assert!(
        updated.skip_shell_integration_prompt,
        "migration lost a completed cooperating config mutation:\n{contents}"
    );
}
