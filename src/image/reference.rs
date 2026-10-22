/// An image reference, like `ubuntu:24.04` or `ghcr.io/owner/image:tag`.
#[derive(Debug, PartialEq)]
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

    /// The host to talk to: Docker Hub's registry isn't at docker.io.
    pub fn host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> (String, String, String) {
        let r = Reference::parse(s);
        (r.registry, r.repo, r.tag)
    }

    #[test]
    fn official_images_live_under_library() {
        assert_eq!(parts("ubuntu:24.04"), ("docker.io".into(), "library/ubuntu".into(), "24.04".into()));
        assert_eq!(parts("ubuntu"), ("docker.io".into(), "library/ubuntu".into(), "latest".into()));
    }

    #[test]
    fn user_images_stay_on_docker_hub() {
        assert_eq!(parts("grafana/grafana:11"), ("docker.io".into(), "grafana/grafana".into(), "11".into()));
    }

    #[test]
    fn a_dotted_or_ported_first_part_is_a_registry() {
        assert_eq!(parts("ghcr.io/owner/img:v1"), ("ghcr.io".into(), "owner/img".into(), "v1".into()));
        assert_eq!(parts("localhost:5000/img"), ("localhost:5000".into(), "img".into(), "latest".into()));
    }

    #[test]
    fn keys_are_flat_directory_names() {
        let key = Reference::parse("ghcr.io/owner/img:v1").key();
        assert_eq!(key, "ghcr.io_owner_img_v1");
    }

    #[test]
    fn docker_hub_is_served_from_registry_1() {
        assert_eq!(Reference::parse("ubuntu").host(), "registry-1.docker.io");
        assert_eq!(Reference::parse("ghcr.io/a/b").host(), "ghcr.io");
    }
}
