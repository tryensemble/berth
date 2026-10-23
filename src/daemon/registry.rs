//! A pull-through registry mirror per tenant, for the Docker in its berths.
//!
//! Each tenant's mirror is an ordinary berth made from the `registry:3`
//! image, running the registry in proxy mode as a service, its port 5000
//! published on the berths' gateway. Berths of that tenant may reach that
//! port and no other tenant's, and their Docker is told to pull through it:
//! what one berth pulled, the next gets from the cache, and Docker Hub's rate
//! limits apply once per tenant, not once per berth. The mirror only caches
//! what upstream sent, by digest; berths can't push to it.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::bail;

use super::berth::CreateSpec;
use super::{Daemon, paths};
use crate::api::ExecSpec;
use crate::net::GATEWAY;

const IMAGE: &str = "registry:3";
const CONFIG: &str = "/etc/distribution/config.yml";
/// Where mirrors are published on the gateway.
const PORTS: std::ops::Range<u16> = 5100..6000;

/// Whether berths' Docker pulls go through a mirror: unless BERTH_REGISTRY is
/// "off".
pub fn enabled() -> bool {
    std::env::var("BERTH_REGISTRY").map_or(true, |v| v != "off")
}

/// The registry mirrors proxy: BERTH_REGISTRY_UPSTREAM, or Docker Hub.
fn upstream() -> String {
    std::env::var("BERTH_REGISTRY_UPSTREAM").unwrap_or_else(|_| "https://registry-1.docker.io".into())
}

/// Whether a berth made of image and layers has Docker in it.
pub fn has_docker(image: &str, layers: &[String]) -> bool {
    let dockerd = Path::new("usr/bin/dockerd");
    crate::image::rootfs(&paths::state("images"), image).join(dockerd).exists()
        || layers.iter().any(|l| paths::state("layers").join(l).join(dockerd).exists())
}

/// The name of a tenant's mirror berth.
pub fn name(tenant: Option<&str>) -> String {
    format!("berth-registry-{}", tenant.unwrap_or("default"))
}

/// The address a berth's Docker uses for the mirror on host port port.
pub fn url(port: u16) -> String {
    format!("http://{GATEWAY}:{port}")
}

impl Daemon {
    /// The gateway port of tenant's mirror, made if there's none yet.
    pub fn registry_for(self: &Arc<Self>, tenant: Option<&str>) -> anyhow::Result<u16> {
        let _one = self.registry_lock.lock().unwrap();
        let name = name(tenant);
        let existing = self
            .berths
            .lock()
            .unwrap()
            .values()
            .find(|b| b.name.as_deref() == Some(&name) && b.tenant.as_deref() == tenant)
            .cloned();
        if let Some(b) = existing {
            if let Some(p) = b.published.iter().find(|p| p.port == 5000) {
                return Ok(p.host_port);
            }
            bail!("the registry berth {} publishes no port; destroy it and it's made again", b.id);
        }
        let spec = CreateSpec {
            image: IMAGE.into(),
            cpus: 1.0,
            memory: "512M".into(),
            name: Some(name),
            tenant: tenant.map(String::from),
            ..serde_json::from_value(serde_json::json!({}))?
        };
        let b = self.create(spec, None)?;
        let made = (|| -> anyhow::Result<u16> {
            let service = ExecSpec {
                argv: ["registry", "serve", CONFIG].map(String::from).to_vec(),
                env: vec![
                    format!("REGISTRY_PROXY_REMOTEURL={}", upstream()),
                    "REGISTRY_HTTP_ADDR=:5000".into(),
                    "REGISTRY_LOG_LEVEL=warn".into(),
                    "OTEL_TRACES_EXPORTER=none".into(),
                ],
                service: Some("registry".into()),
                ..Default::default()
            };
            self.add_service(&b.id, service)?;
            self.expose(&b.id);
            for port in PORTS {
                if self.publish(&b.id, 5000, IpAddr::V4(GATEWAY), port).is_ok() {
                    return Ok(port);
                }
            }
            bail!("no free port for the registry mirror in {PORTS:?}")
        })();
        if made.is_err() {
            let _ = self.destroy(&b.id);
        }
        made
    }
}
