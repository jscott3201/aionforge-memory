//! Static operator console serving for the CLI HTTP process.

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Method, Request, Response, StatusCode};
use tower::service_fn;
use tower_http::services::ServeDir;

pub(crate) const BASE_PATH: &str = "/console";

const DIST_ENV: &str = "AIONFORGE_CONSOLE_DIST_DIR";
const REPO_DIST_DIR: &str = "ui/console/build";
const RELEASE_DIST_DIR: &str = "console";
const CONTAINER_DIST_DIR: &str = "/usr/local/share/aionforge/console";
const DEFAULT_DIST_HINT: &str =
    "ui/console/build, executable-adjacent console/, ./console, /usr/local/share/aionforge/console";
const SPA_SHELL: &str = "200.html";

pub(crate) fn resolve_dist_dir() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(DIST_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return dist_dir_with_shell(configured);
    }

    default_dist_dirs()
        .into_iter()
        .find_map(dist_dir_with_shell)
}

fn default_dist_dirs() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(REPO_DIST_DIR)];
    if let Some(exe_parent) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_parent.join(RELEASE_DIST_DIR));
    }
    candidates.push(PathBuf::from(RELEASE_DIST_DIR));
    candidates.push(PathBuf::from(CONTAINER_DIST_DIR));
    candidates
}

fn dist_dir_with_shell(path: PathBuf) -> Option<PathBuf> {
    path.join(SPA_SHELL).is_file().then_some(path)
}

pub(crate) fn report_startup(console_dist: Option<&Path>) {
    match console_dist {
        Some(path) => {
            tracing::info!(
                target: "aionforge::serve",
                base_path = BASE_PATH,
                dist_dir = %path.display(),
                "operator console enabled",
            );
        }
        None => {
            tracing::info!(
                target: "aionforge::serve",
                base_path = BASE_PATH,
                env = DIST_ENV,
                default_dist_dirs = DEFAULT_DIST_HINT,
                "operator console disabled; build assets or set the console dist env",
            );
        }
    }
}

pub(crate) fn mount<S>(router: Router<S>, console_dist: Option<PathBuf>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let Some(console_dist) = console_dist else {
        return router;
    };

    let shell = console_dist.join(SPA_SHELL);
    let fallback = service_fn(move |request: Request<Body>| {
        let shell = shell.clone();
        async move { Ok::<_, Infallible>(fallback_response(request, shell).await) }
    });
    let console = ServeDir::new(console_dist)
        .append_index_html_on_directories(false)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(fallback);
    router.nest_service(BASE_PATH, console)
}

async fn fallback_response(request: Request<Body>, shell: PathBuf) -> Response<Body> {
    if !is_shell_method(request.method()) {
        return plain_body_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    }
    if !is_spa_route(request.uri().path()) {
        return plain_body_response(StatusCode::NOT_FOUND, "Not Found");
    }

    match tokio::fs::read(&shell).await {
        Ok(bytes) => shell_response(request.method(), bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plain_body_response(StatusCode::NOT_FOUND, "Not Found")
        }
        Err(error) => {
            tracing::error!(
                target: "aionforge::serve",
                error = %error,
                shell = %shell.display(),
                "failed to read operator console shell",
            );
            plain_body_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

fn is_spa_route(path: &str) -> bool {
    let Some(relative) = clean_relative_path(path) else {
        return false;
    };
    if relative.as_os_str().is_empty() {
        return true;
    }
    !has_app_asset_segment(&relative) && relative.extension().is_none()
}

fn is_shell_method(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD
}

fn has_app_asset_segment(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "_app")
}

fn clean_relative_path(path: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(segment) => {
                let text = segment.to_str()?;
                if text.contains('\\') {
                    return None;
                }
                relative.push(segment);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

fn shell_response(method: &Method, bytes: Vec<u8>) -> Response<Body> {
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header(CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("valid console shell response")
}

fn plain_body_response(status: StatusCode, body: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .expect("valid plain response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dist_dirs_cover_source_archive_and_container_layouts() {
        let candidates = default_dist_dirs();

        assert_eq!(candidates.first(), Some(&PathBuf::from(REPO_DIST_DIR)));
        assert!(candidates.contains(&PathBuf::from(RELEASE_DIST_DIR)));
        assert!(candidates.contains(&PathBuf::from(CONTAINER_DIST_DIR)));
    }

    #[test]
    fn spa_shell_only_answers_navigation_methods() {
        assert!(is_shell_method(&Method::GET));
        assert!(is_shell_method(&Method::HEAD));
        assert!(!is_shell_method(&Method::POST));
        assert!(!is_shell_method(&Method::DELETE));
    }

    #[test]
    fn spa_route_classifier_separates_routes_from_assets() {
        for route in ["/", "", "/records", "/records/", "/records/detail"] {
            assert!(
                is_spa_route(route),
                "{route:?} should fall back to the SPA shell"
            );
        }

        for asset in [
            "/_app/immutable/app.js",
            "/_app/immutable",
            "/console/_app/immutable",
            "/_app/version.json",
            "/favicon.svg",
            "/records.json",
            "/../secret",
            "/records\\secret",
        ] {
            assert!(
                !is_spa_route(asset),
                "{asset:?} should not fall back to the SPA shell"
            );
        }
    }
}
