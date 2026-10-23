//! Projects' repositories, mirrored once for all their berths.
//!
//! git talks to whatever server or path a caller names, so the daemon never
//! runs it as root: fetches run as an id of their own that owns nothing but
//! the mirrors, may write only into the mirror being fetched, and speak only
//! git's ordinary transports.

use std::fs;
use std::os::unix::fs::chown;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

use super::berth::Berth;
use super::spawn::exec_collect;
use super::{UID_START, paths, valid_name};
use crate::api::ExecSpec;
use crate::sys::{self, landlock};

/// Runs git's upload-pack whoever owns the repository: a mirror is owned by
/// FETCH_UID, and inside a berth by an id the berth doesn't map.
pub const UPLOAD_PACK: &str = "git -c safe.directory='*' upload-pack";

/// The id mirrors are fetched as and owned by: just below the berths' ranges,
/// used by nothing else. It owns the repos directory too, so it can write
/// mirrors there while the directory stays closed to other host users.
pub const FETCH_UID: u32 = UID_START - 1;

/// Transports a mirror may come over: not ext:: (it runs commands) or the
/// other unusual ones.
const PROTOCOLS: &[&str] = &[
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.http.allow=always",
    "protocol.ssh.allow=always",
    "protocol.git.allow=always",
    "protocol.file.allow=always",
];

/// Mirrors url as project name, or fetches it again.
pub fn add_repo(name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        bail!("bad project name {name}");
    }
    if url.starts_with('-') || url.is_empty() {
        bail!("bad repository url {url}");
    }
    let path = paths::state("repos").join(format!("{name}.git"));
    if path.exists() {
        let p = path.display().to_string();
        git(&path, &["--git-dir", &p, "remote", "set-url", "origin", url])?;
        git(
            &path,
            &["--git-dir", &p, "-c", &format!("remote.origin.uploadpack={UPLOAD_PACK}"), "remote", "update", "--prune"],
        )?;
        return Ok(());
    }
    // Fetched next to where it goes, so a failed clone leaves nothing.
    let tmp = paths::state("repos").join(format!(".{name}.{}", super::berth::random_id()));
    fs::create_dir(&tmp)?;
    chown(&tmp, Some(FETCH_UID), Some(FETCH_UID))?;
    let result = (|| -> anyhow::Result<()> {
        let into = tmp.join("repo.git").display().to_string();
        git(&tmp, &["clone", "-q", "--mirror", "--no-local", "--upload-pack", UPLOAD_PACK, "--", url, &into])?;
        fs::rename(tmp.join("repo.git"), &path)?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Runs git as FETCH_UID, writing only beneath writable.
fn git(writable: &Path, args: &[&str]) -> anyhow::Result<String> {
    let ruleset = landlock::Ruleset::new(&[writable, Path::new("/dev/null")])?;
    if ruleset.is_none() {
        landlock::warn_once();
    }
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("safe.directory=*");
    for p in PROTOCOLS {
        cmd.arg("-c").arg(p);
    }
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", writable)
        .env("TMPDIR", writable)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new")
        .current_dir(writable)
        .stdin(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            libc::umask(0o022);
            sys::cvt(libc::setgroups(0, std::ptr::null()))?;
            sys::cvt(libc::setresgid(FETCH_UID, FETCH_UID, FETCH_UID))?;
            sys::cvt(libc::setresuid(FETCH_UID, FETCH_UID, FETCH_UID))?;
            sys::cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            if let Some(r) = &ruleset {
                r.restrict_self()?;
            }
            Ok(())
        });
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clones the project into the berth's /workspace as the agent, sharing the
/// mirror's objects, on the berth's branch, with the mirror's origin.
pub fn check_out(b: &Berth, project: &str, mirror: &Path) -> anyhow::Result<()> {
    let origin = sys::run(
        "git",
        &["--git-dir", &mirror.display().to_string(), "-c", "safe.directory=*", "config", "--get", "remote.origin.url"],
    )
    .unwrap_or_default();
    const SCRIPT: &str = r#"set -e
git -c safe.directory='*' clone -q --shared -u "$1" /berth/repo.git /workspace
cd /workspace
if [ -n "$2" ]; then git checkout -q "$2" 2>/dev/null || git checkout -q -b "$2"; fi
if [ -n "$3" ]; then git remote set-url origin "$3"; fi"#;
    let args = [SCRIPT, "sh", UPLOAD_PACK, b.branch.as_deref().unwrap_or(""), origin.trim()];
    let spec =
        ExecSpec { argv: ["sh", "-c"].into_iter().chain(args).map(String::from).collect(), ..Default::default() };
    let (code, output) = exec_collect(b, &spec)?;
    if code != 0 {
        bail!("checking out {project} failed: {}", output.trim());
    }
    Ok(())
}
