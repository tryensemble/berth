//! Pulls OCI images from a registry and unpacks them into a root filesystem,
//! without Docker.

mod reference;
mod registry;
mod unpack;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use reference::Reference;
use registry::{MANIFEST_TYPES, Registry, arch};
pub use unpack::child as unpack_child;

/// Where reference's root filesystem is kept under images.
pub fn rootfs(images: &Path, reference: &str) -> PathBuf {
    images.join(Reference::parse(reference).key()).join("rootfs")
}

/// Pulls reference into `images/<key>/rootfs`, unless it's there already.
///
/// What a registry sent is handled with as little power as possible: it's
/// downloaded in a thread that may write only into the image's directory, and
/// each layer is unpacked by its own process (see [`unpack::isolated`]), as
/// the unprivileged ids userns maps to, with what it writes shifted back to
/// ids from 0 on the way to disk.
pub fn pull(images: &Path, reference: &str, userns: &OwnedFd) -> anyhow::Result<PathBuf> {
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
    let (reference, target) = (reference.to_string(), dir.clone());
    let fetched = crate::sys::landlock::confined(&[&dir], move || fetch(&target, &r, &reference));
    let Ok((manifest, layers)) = fetched else {
        remove_blobs(&dir);
        return fetched.map(|_| root);
    };

    let tmp = dir.join("rootfs.partial");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let unpacked = (|| -> anyhow::Result<()> {
        let tree =
            crate::sys::idmapped_tree(&tmp, userns.as_raw_fd(), false).context("mounting the image to unpack")?;
        for layer in &layers {
            unpack::isolated(&tree, userns, &layer.blob, layer.gzip)
                .with_context(|| format!("unpacking {}", layer.digest))?;
        }
        Ok(())
    })();
    remove_blobs(&dir);
    unpacked?;
    fs::rename(&tmp, &root)?;
    fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(root)
}

/// Removes the downloaded layers in dir.
fn remove_blobs(dir: &Path) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("blob") {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// A layer downloaded and checked against its digest.
struct Layer {
    digest: String,
    blob: PathBuf,
    gzip: bool,
}

/// Fetches reference's manifest and its layers' blobs into dir.
fn fetch(dir: &Path, r: &Reference, reference: &str) -> anyhow::Result<(Value, Vec<Layer>)> {
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
    let mut layers = Vec::new();
    for (i, layer) in manifest["layers"].as_array().context("manifest without layers")?.iter().enumerate() {
        let digest = layer["digest"].as_str().context("layer without digest")?.to_string();
        let media = layer["mediaType"].as_str().unwrap_or("");
        if media.contains("zstd") {
            bail!("zstd layers aren't supported yet ({digest})");
        }
        let blob = dir.join(format!("blob{i}"));
        let mut file = File::create(&blob)?;
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
        layers.push(Layer { digest, blob, gzip: media.contains("gzip") });
    }
    Ok((manifest, layers))
}
