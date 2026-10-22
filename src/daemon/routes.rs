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
    /// `POST /v1/berths/{id}/start`
    Start(&'a str),
    /// `POST /v1/berths/{id}/idle`
    Idle(&'a str),
    /// `POST /v1/berths/{id}/stop`
    Stop(&'a str),
    /// `GET /v1/berths/{id}/services`
    Services(&'a str),
    /// `DELETE /v1/berths/{id}/services/{name}`
    RemoveService(&'a str, &'a str),
    /// `POST /v1/images`
    PullImage,
    /// `GET /v1/images`
    Images,
    /// `POST /v1/repos`
    AddRepo,
    /// `GET /v1/repos`
    Repos,
    /// `DELETE /v1/repos/{name}`
    RemoveRepo(&'a str),
    /// `POST /v1/layers`
    CommitLayer,
    /// `GET /v1/layers`
    Layers,
    /// `DELETE /v1/layers/{name}`
    RemoveLayer(&'a str),
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
            ("POST", ["v1", "berths", id, "start"]) => Route::Start(id),
            ("POST", ["v1", "berths", id, "idle"]) => Route::Idle(id),
            ("POST", ["v1", "berths", id, "stop"]) => Route::Stop(id),
            ("GET", ["v1", "berths", id, "services"]) => Route::Services(id),
            ("DELETE", ["v1", "berths", id, "services", name]) => Route::RemoveService(id, name),
            ("POST", ["v1", "images"]) => Route::PullImage,
            ("GET", ["v1", "images"]) => Route::Images,
            ("POST", ["v1", "repos"]) => Route::AddRepo,
            ("GET", ["v1", "repos"]) => Route::Repos,
            ("DELETE", ["v1", "repos", name]) => Route::RemoveRepo(name),
            ("POST", ["v1", "layers"]) => Route::CommitLayer,
            ("GET", ["v1", "layers"]) => Route::Layers,
            ("DELETE", ["v1", "layers", name]) => Route::RemoveLayer(name),
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
    fn lifecycle_and_service_routes() {
        assert_eq!(Route::parse("POST", "/v1/berths/ab/start", false), Some(Route::Start("ab")));
        assert_eq!(Route::parse("POST", "/v1/berths/ab/idle", false), Some(Route::Idle("ab")));
        assert_eq!(Route::parse("POST", "/v1/berths/ab/stop", false), Some(Route::Stop("ab")));
        assert_eq!(Route::parse("GET", "/v1/berths/ab/services", false), Some(Route::Services("ab")));
        assert_eq!(
            Route::parse("DELETE", "/v1/berths/ab/services/web", false),
            Some(Route::RemoveService("ab", "web"))
        );
    }

    #[test]
    fn store_routes() {
        assert_eq!(Route::parse("GET", "/v1/images", false), Some(Route::Images));
        assert_eq!(Route::parse("GET", "/v1/layers", false), Some(Route::Layers));
        assert_eq!(Route::parse("DELETE", "/v1/layers/tools", false), Some(Route::RemoveLayer("tools")));
        assert_eq!(Route::parse("GET", "/v1/repos", false), Some(Route::Repos));
        assert_eq!(Route::parse("DELETE", "/v1/repos/web", false), Some(Route::RemoveRepo("web")));
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
