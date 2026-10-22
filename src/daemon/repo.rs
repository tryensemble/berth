//! Projects' repositories, mirrored once for all their berths.

use std::path::Path;

use anyhow::bail;

use super::berth::Berth;
use super::spawn::exec_collect;
use super::{paths, valid_name};
use crate::api::ExecSpec;
use crate::sys;

/// Runs git's upload-pack whoever owns the repository: a local path may
/// belong to another user, and a mirror inside a berth to an unmapped one.
pub const UPLOAD_PACK: &str = "git -c safe.directory='*' upload-pack";

/// Mirrors url as project name, or fetches it again.
pub fn add_repo(name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        bail!("bad project name {name}");
    }
    let path = paths::state("repos").join(format!("{name}.git"));
    let p = path.display().to_string();
    if path.exists() {
        let upload_pack = format!("remote.origin.uploadpack={UPLOAD_PACK}");
        sys::run("git", &["--git-dir", &p, "remote", "set-url", "origin", url])?;
        sys::run("git", &["--git-dir", &p, "-c", &upload_pack, "remote", "update", "--prune"])?;
    } else {
        sys::run("git", &["-c", "safe.directory=*", "clone", "-q", "--mirror", "--upload-pack", UPLOAD_PACK, url, &p])?;
    }
    Ok(())
}

/// Clones the project into the berth's /workspace as the agent, sharing the
/// mirror's objects, on the berth's branch, with the mirror's origin.
pub fn check_out(b: &Berth, project: &str, mirror: &Path) -> anyhow::Result<()> {
    let origin = sys::run("git", &["--git-dir", &mirror.display().to_string(), "config", "--get", "remote.origin.url"])
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
