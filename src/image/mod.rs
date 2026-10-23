//! Pulls OCI images from a registry and unpacks them into a root filesystem,
//! without Docker.

mod reference;
mod registry;
mod unpack;

use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use reference::Reference;
use registry::{MANIFEST_TYPES, Registry, arch};

/// Where reference's root filesystem is kept under images.
pub fn rootfs(images: &Path, reference: &str) -> PathBuf {
    images.join(Reference::parse(reference).key()).join("rootfs")
}

/// Pulls reference into `images/<key>/rootfs`, unless it's there already.
///
/// Unpacking runs as root over what a registry sent, so it runs in a thread
/// that may write only into that image's directory.
pub fn pull(images: &Path, reference: &str) -> anyhow::Result<PathBuf> {
    // One pull at a time: two of the same image would share a directory.
    static PULLING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = PULLING.lock().unwrap_or_else(|e| e.into_inner());
    let r = Reference::parse(reference);
    let dir = images.join(r.key());
    let root = dir.join("rootfs");
    if root.exists() {
        return Ok(root);
    }
    fs::create_dir_all(&dir)?;
    let reference = reference.to_string();
    let target = dir.clone();
    crate::sys::landlock::confined(&[&dir], move || pull_into(&target, &r, &reference))
}

fn pull_into(dir: &Path, r: &Reference, reference: &str) -> anyhow::Result<PathBuf> {
    let root = dir.join("rootfs");
    let mut reg = Registry::default();
    let base = format!("https://{}/v2/{}", r.host(), r.repo);

    let mut manifest: Value = reg.get(&format!("{base}/manifests/{}", r.tag), MANIFEST_TYPES)?.into_json()?;
    if let Some(list) = manifest.get("manifests").and_then(|m| m.as_array()) {
        let digest = list
            .iter()
            .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == arch())
            .and_then(|m| m["digest"].as_str())
            .with_context(|| format!("{reference} has no linux/{} image", arch()))?
            .to_string();
        manifest = reg.get(&format!("{base}/manifests/{digest}"), MANIFEST_TYPES)?.into_json()?;
    }
    let layers = manifest["layers"].as_array().context("manifest without layers")?.clone();

    let tmp = dir.join("rootfs.partial");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    for layer in layers {
        let digest = layer["digest"].as_str().context("layer without digest")?;
        let media = layer["mediaType"].as_str().unwrap_or("");
        if media.contains("zstd") {
            bail!("zstd layers aren't supported yet ({digest})");
        }
        let blob = dir.join("blob");
        let mut file = File::options().create(true).truncate(true).read(true).write(true).open(&blob)?;
        let mut resp = reg.get(&format!("{base}/blobs/{digest}"), "*/*")?.into_reader();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
        }
        let got = format!("sha256:{:x}", hasher.finalize());
        if got != digest {
            bail!("layer {digest} arrived as {got}");
        }
        file.rewind()?;
        unpack::unpack_layer(file, &tmp, media.contains("gzip")).with_context(|| format!("unpacking {digest}"))?;
        fs::remove_file(&blob)?;
    }
    fs::rename(&tmp, &root)?;
    fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(root)
}
