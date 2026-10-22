//! Pulls OCI images from a registry and unpacks them into a root filesystem,
//! without Docker.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct Reference {
    pub registry: String,
    pub repo: String,
    pub tag: String,
}

impl Reference {
    pub fn parse(s: &str) -> Reference {
        let (name, tag) = match s.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, t),
            _ => (s, "latest"),
        };
        let mut parts = name.splitn(2, '/');
        let first = parts.next().unwrap();
        let (registry, repo) = match parts.next() {
            Some(rest) if first.contains('.') || first.contains(':') => (first.to_string(), rest.to_string()),
            Some(rest) => ("docker.io".to_string(), format!("{first}/{rest}")),
            None => ("docker.io".to_string(), format!("library/{first}")),
        };
        Reference { registry, repo, tag: tag.to_string() }
    }

    /// The directory name an image is kept under.
    pub fn key(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repo, self.tag).replace(['/', ':'], "_")
    }

    fn host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }
}

pub fn rootfs(images: &Path, reference: &str) -> PathBuf {
    images.join(Reference::parse(reference).key()).join("rootfs")
}

struct Registry {
    agent: ureq::Agent,
    token: Option<String>,
}

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

impl Registry {
    fn get(&mut self, url: &str, accept: &str) -> anyhow::Result<ureq::Response> {
        for _ in 0..2 {
            let mut req = self.agent.get(url).set("Accept", accept);
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            match req.call() {
                Ok(r) => return Ok(r),
                Err(ureq::Error::Status(401, r)) if self.token.is_none() => {
                    let challenge = r.header("www-authenticate").unwrap_or("").to_string();
                    self.token = Some(self.authenticate(&challenge)?);
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("the registry refused {url}")
    }

    /// Gets an anonymous token for a Bearer challenge.
    fn authenticate(&self, challenge: &str) -> anyhow::Result<String> {
        let params = challenge.strip_prefix("Bearer ").context("unsupported registry authentication")?;
        let mut realm = String::new();
        let mut query = Vec::new();
        for part in params.split(',') {
            let Some((k, v)) = part.split_once('=') else { continue };
            let v = v.trim_matches('"').to_string();
            if k.trim() == "realm" {
                realm = v;
            } else {
                query.push((k.trim().to_string(), v));
            }
        }
        let mut req = self.agent.get(&realm);
        for (k, v) in &query {
            req = req.query(k, v);
        }
        let v: Value = req.call()?.into_json()?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .context("no token from the registry")
    }
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    }
}

/// Pulls reference into images/<key>/rootfs, unless it's there already.
pub fn pull(images: &Path, reference: &str) -> anyhow::Result<PathBuf> {
    let r = Reference::parse(reference);
    let dir = images.join(r.key());
    let root = dir.join("rootfs");
    if root.exists() {
        return Ok(root);
    }
    let mut reg = Registry { agent: ureq::AgentBuilder::new().redirects(5).build(), token: None };
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
        unpack_layer(file, &tmp, media.contains("gzip"))
            .with_context(|| format!("unpacking {digest}"))?;
        fs::remove_file(&blob)?;
    }
    fs::rename(&tmp, &root)?;
    fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(root)
}

fn unpack_layer(file: File, root: &Path, gzip: bool) -> anyhow::Result<()> {
    let reader: Box<dyn Read> = if gzip { Box::new(flate2::read::GzDecoder::new(file)) } else { Box::new(file) };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_unpack_xattrs(true);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(hidden) = name.strip_prefix(".wh.") {
            let parent = root.join(path.parent().unwrap_or(Path::new("")));
            if hidden == ".wh..opq" {
                if let Ok(rd) = fs::read_dir(&parent) {
                    for e in rd.flatten() {
                        remove_any(&e.path())?;
                    }
                }
            } else {
                remove_any(&parent.join(hidden))?;
            }
            continue;
        }
        entry.unpack_in(root)?;
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}
