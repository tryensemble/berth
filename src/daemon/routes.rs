//! The daemon's endpoints.

/// An API request, resolved from its method and path.
#[derive(Debug, PartialEq)]
pub enum Route<'a> {
    /// `GET /v1/berths`
    List,
    /// `POST /v1/berths`
    Create,
    /// `GET /v1/berths/{id}`
    Get(&'a str),
    /// `DELETE /v1/berths/{id}`
    Destroy(&'a str),
    /// `GET /v1/berths/{id}/ports`
    Ports(&'a str),
    /// `POST /v1/berths/{id}/exec`, upgraded to an exec stream
    Exec(&'a str),
    /// `POST /v1/berths/{id}/proxy`, upgraded to a byte stream to a port
    Proxy(&'a str),
    /// `POST /v1/images`
    PullImage,
    /// `POST /v1/repos`
    AddRepo,
    /// `POST /v1/layers`
    CommitLayer,
}

impl<'a> Route<'a> {
    pub fn parse(method: &str, path: &'a str, upgrade: bool) -> Option<Route<'a>> {
        let path = path.split('?').next().unwrap_or(path);
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        Some(match (method, parts.as_slice()) {
            ("GET", ["v1", "berths"]) => Route::List,
            ("POST", ["v1", "berths"]) => Route::Create,
            ("GET", ["v1", "berths", id]) => Route::Get(id),
            ("DELETE", ["v1", "berths", id]) => Route::Destroy(id),
            ("GET", ["v1", "berths", id, "ports"]) => Route::Ports(id),
            ("POST", ["v1", "berths", id, "exec"]) if upgrade => Route::Exec(id),
            ("POST", ["v1", "berths", id, "proxy"]) if upgrade => Route::Proxy(id),
            ("POST", ["v1", "images"]) => Route::PullImage,
            ("POST", ["v1", "repos"]) => Route::AddRepo,
            ("POST", ["v1", "layers"]) => Route::CommitLayer,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn berths_routes() {
        assert_eq!(Route::parse("GET", "/v1/berths", false), Some(Route::List));
        assert_eq!(Route::parse("POST", "/v1/berths", false), Some(Route::Create));
        assert_eq!(Route::parse("GET", "/v1/berths/ab12", false), Some(Route::Get("ab12")));
        assert_eq!(Route::parse("DELETE", "/v1/berths/ab12", false), Some(Route::Destroy("ab12")));
        assert_eq!(Route::parse("GET", "/v1/berths/ab12/ports", false), Some(Route::Ports("ab12")));
        assert_eq!(Route::parse("GET", "/v1/berths/?all", false), Some(Route::List));
    }

    #[test]
    fn streams_need_an_upgrade() {
        assert_eq!(Route::parse("POST", "/v1/berths/ab12/exec", true), Some(Route::Exec("ab12")));
        assert_eq!(Route::parse("POST", "/v1/berths/ab12/proxy", true), Some(Route::Proxy("ab12")));
        assert_eq!(Route::parse("POST", "/v1/berths/ab12/exec", false), None);
    }

    #[test]
    fn unknown_requests_have_no_route() {
        assert_eq!(Route::parse("PUT", "/v1/berths", false), None);
        assert_eq!(Route::parse("GET", "/v2/berths", false), None);
        assert_eq!(Route::parse("GET", "/v1/berths/a/b/c", false), None);
    }
}
