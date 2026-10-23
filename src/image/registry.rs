//! A client for OCI distribution registries, with anonymous token auth.

use anyhow::{Context, bail};
use serde_json::Value;

/// The manifest kinds berth understands: indexes (multi-platform) and images.
pub const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

pub struct Registry {
    agent: ureq::Agent,
    token: Option<String>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry { agent: ureq::AgentBuilder::new().redirects(5).build(), token: None }
    }
}

impl Registry {
    /// GETs url, getting a token first when the registry asks for one.
    pub fn get(&mut self, url: &str, accept: &str) -> anyhow::Result<ureq::Response> {
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
        let (realm, query) = parse_challenge(challenge).context("unsupported registry authentication")?;
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

/// The realm and the other parameters of a `WWW-Authenticate: Bearer ...`
/// challenge.
fn parse_challenge(challenge: &str) -> Option<(String, Vec<(String, String)>)> {
    let params = challenge.strip_prefix("Bearer ")?;
    // Split on the commas outside quotes: a scope may list several actions.
    let mut parts = vec![String::new()];
    let mut quoted = false;
    for c in params.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => parts.push(String::new()),
            c => parts.last_mut().unwrap().push(c),
        }
    }
    let mut realm = None;
    let mut rest = Vec::new();
    for (k, v) in parts.iter().filter_map(|p| p.split_once('=')) {
        match k.trim() {
            "realm" => realm = Some(v.to_string()),
            k => rest.push((k.to_string(), v.to_string())),
        }
    }
    Some((realm?, rest))
}

/// The running machine's architecture, as OCI platforms name it.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn docker_hub_challenges_parse() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/ubuntu:pull""#;
        let (realm, params) = parse_challenge(c).unwrap();
        assert_eq!(realm, "https://auth.docker.io/token");
        assert_eq!(params, pairs(&[("service", "registry.docker.io"), ("scope", "repository:library/ubuntu:pull")]));
    }

    #[test]
    fn quoted_values_may_hold_commas() {
        let (_, params) = parse_challenge(r#"Bearer realm="r",scope="repository:a/b:pull,push""#).unwrap();
        assert_eq!(params, pairs(&[("scope", "repository:a/b:pull,push")]));
    }

    #[test]
    fn parts_without_values_are_skipped() {
        let (realm, params) = parse_challenge(r#"Bearer realm="r", error, service="s""#).unwrap();
        assert_eq!(realm, "r");
        assert_eq!(params, pairs(&[("service", "s")]));
    }

    #[test]
    fn other_schemes_and_missing_realms_are_refused() {
        assert!(parse_challenge(r#"Basic realm="x""#).is_none());
        assert!(parse_challenge(r#"Bearer service="x""#).is_none());
    }
}
