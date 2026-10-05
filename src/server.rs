use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Extension, Query, Request, State, ws::WebSocketUpgrade,
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::Ordering,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;
use url::Url;

use crate::{
    auth::{self, Auth, Session},
    config::{self, Cli, Config},
    index::{Index, SearchQuery},
    terminal,
};

pub struct App {
    pub config: Mutex<Config>,
    pub data_dir: PathBuf,
    pub index: Arc<Index>,
    pub auth: Auth,
    pub security: Security,
    pub shell: PathBuf,
    pub auto_index: bool,
    pub terminals: Arc<Semaphore>,
    pub searches: Arc<Semaphore>,
    pub logins: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct Security {
    pub origins: Vec<String>,
    pub hosts: Vec<String>,
    pub secure_cookie: bool,
}

impl Security {
    pub fn new(cli: &Cli) -> Result<Self> {
        if cli.bind.port() == 0 {
            bail!("Choose a fixed nonzero TCP port");
        }
        if !cli.bind.ip().is_loopback() && cli.tls_cert.is_none() && !cli.allow_insecure_http {
            bail!(
                "Non-loopback HTTP exposes your password and shell. Use --tls-cert/--tls-key, a loopback HTTPS proxy, or explicitly opt in with --allow-insecure-http."
            );
        }
        let scheme = if cli.tls_cert.is_some() {
            "https"
        } else {
            "http"
        };
        let mut origins = Vec::new();
        if let Some(public) = &cli.public_url {
            let url = Url::parse(public)?;
            if !["http", "https"].contains(&url.scheme())
                || url.host_str().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                bail!(
                    "--public-url must be a plain HTTP(S) origin without a path, credentials or query"
                );
            }
            if cli.tls_cert.is_some() && url.scheme() != "https" {
                bail!("TLS requires an https public URL");
            }
            origins.push(url.origin().ascii_serialization());
        } else {
            if !cli.bind.ip().is_loopback() {
                bail!("For remote access, set --public-url to the exact browser origin");
            }
            for host in [
                cli.bind.ip().to_string(),
                "localhost".into(),
                "127.0.0.1".into(),
                "[::1]".into(),
            ] {
                let host = if host.contains(':') && !host.starts_with('[') {
                    format!("[{host}]")
                } else {
                    host
                };
                origins.push(
                    Url::parse(&format!("{scheme}://{host}:{}", cli.bind.port()))?
                        .origin()
                        .ascii_serialization(),
                );
            }
            origins.sort();
            origins.dedup();
        }
        let secure_cookie = origins.iter().all(|o| o.starts_with("https://"));
        let hosts = origins
            .iter()
            .map(|origin| origin.split_once("://").unwrap().1.to_owned())
            .collect();
        Ok(Self {
            origins,
            hosts,
            secure_cookie,
        })
    }

    fn valid_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get("origin")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|o| self.origins.iter().any(|allowed| allowed == o))
    }
}

pub struct ApiError(pub StatusCode, pub String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
fn error(status: StatusCode, message: &str) -> ApiError {
    ApiError(status, message.into())
}
fn internal(err: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %err, "Request failed");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Operation failed; check the server log",
    )
}

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/api/session", get(session))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/search", get(search))
        .route("/api/settings", get(settings).put(update_settings))
        .route("/api/index", post(reindex))
        .route("/api/index/cancel", post(cancel_index))
        .route("/api/terminal", get(websocket))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    Router::new()
        .merge(protected)
        .route("/api/login", post(login))
        .route(
            "/licenses",
            get(|| async {
                asset("text/plain; charset=utf-8", concat!(
                "Embedded browser libraries: @xterm/xterm 5.5.0 and @xterm/addon-fit 0.10.0\n\n",
                include_str!("../web/vendor/LICENSE.xterm"), "\n\n",
                include_str!("../web/vendor/LICENSE.addon-fit")
            ).as_bytes())
            }),
        )
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_bytes!("../web/index.html"),
                )
            }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", include_bytes!("../web/app.css")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_bytes!("../web/app.js"),
                )
            }),
        )
        .route(
            "/vendor/xterm.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_bytes!("../web/vendor/xterm.js"),
                )
            }),
        )
        .route(
            "/vendor/xterm.css",
            get(|| async {
                asset(
                    "text/css; charset=utf-8",
                    include_bytes!("../web/vendor/xterm.css"),
                )
            }),
        )
        .route(
            "/vendor/addon-fit.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_bytes!("../web/vendor/addon-fit.js"),
                )
            }),
        )
        .fallback(|| async { error(StatusCode::NOT_FOUND, "Not found") })
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            security_headers,
        ))
        .with_state(app)
}

fn asset(content_type: &'static str, data: &'static [u8]) -> Response {
    ([("content-type", content_type)], data).into_response()
}

async fn security_headers(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()))
        .unwrap_or("");
    let invalid_host = !app
        .security
        .hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host));
    let invalid_origin = (!matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) || request.uri().path() == "/api/terminal")
        && !app.security.valid_origin(request.headers());
    let mut response = if invalid_host {
        error(StatusCode::FORBIDDEN, "Host is not allowed").into_response()
    } else if invalid_origin {
        error(StatusCode::FORBIDDEN, "Origin is not allowed").into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    for (key, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'",
        ),
    ] {
        headers.insert(key, HeaderValue::from_static(value));
    }
    if app.security.secure_cookie {
        headers.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

async fn authorize(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    let Some(session) = app.auth.session(request.headers()) else {
        return error(StatusCode::UNAUTHORIZED, "Please unlock to continue").into_response();
    };
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && request
            .headers()
            .get("x-csrf-token")
            .and_then(|h| h.to_str().ok())
            != Some(session.csrf.as_str())
    {
        return error(StatusCode::FORBIDDEN, "Invalid CSRF token").into_response();
    }
    request.extensions_mut().insert(session);
    next.run(request).await
}

#[derive(Deserialize)]
struct Login {
    password: String,
}
async fn login(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Result<Response, ApiError> {
    if !app.auth.allow_login(peer.ip()) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Please wait 60 seconds.",
        ));
    }
    let permit = app.logins.clone().try_acquire_owned().map_err(|_| {
        error(
            StatusCode::TOO_MANY_REQUESTS,
            "Login is busy. Please try again shortly.",
        )
    })?;
    let hash = app.config.lock().unwrap().password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth::verify_password(&input.password, &hash)
    })
    .await
    .map_err(internal)?;
    if !valid {
        return Err(error(StatusCode::UNAUTHORIZED, "Incorrect password"));
    }
    // Re-authentication rotates the current session instead of accumulating cookies.
    app.auth.logout(&headers);
    let (token, session) = app
        .auth
        .create()
        .ok_or_else(|| error(StatusCode::TOO_MANY_REQUESTS, "Too many active sessions"))?;
    let mut response = Json(json!({"csrf": session.csrf})).into_response();
    response.headers_mut().insert(
        "set-cookie",
        auth::cookie(&token, app.security.secure_cookie)
            .parse()
            .map_err(internal)?,
    );
    Ok(response)
}

async fn session(Extension(session): Extension<Arc<Session>>) -> Json<Value> {
    Json(json!({"csrf": session.csrf}))
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    app.auth.logout(&headers);
    let mut response = Json(json!({"ok": true})).into_response();
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        auth::COOKIE_NAME,
        if app.security.secure_cookie {
            "; Secure"
        } else {
            ""
        }
    );
    response
        .headers_mut()
        .insert("set-cookie", cookie.parse().unwrap());
    response
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    let progress = app.index.progress.lock().unwrap().clone();
    let roots_count = app.config.lock().unwrap().roots.len();
    Json(json!({"index": progress, "os": std::env::consts::OS,
        "shell": config::display_path(&app.shell), "roots_count": roots_count, "auto_index": app.auto_index }))
}

async fn search(
    State(app): State<Arc<App>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<crate::index::SearchResult>, ApiError> {
    let permit = app.searches.clone().try_acquire_owned().map_err(|_| {
        error(
            StatusCode::TOO_MANY_REQUESTS,
            "Search is busy; try again shortly",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        app.index
            .search(&query)
            .map(Json)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("Search failed: {e}")))
    })
    .await
    .map_err(internal)?
}

async fn settings(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.config.lock().unwrap();
    Json(
        json!({"roots": cfg.roots.iter().map(|p| config::display_path(p)).collect::<Vec<_>>(),
        "excludes": cfg.excludes, "index_interval_minutes": cfg.index_interval_minutes }),
    )
}

#[derive(Deserialize)]
struct Settings {
    roots: Vec<PathBuf>,
    excludes: Vec<String>,
    index_interval_minutes: u64,
}

async fn update_settings(
    State(app): State<Arc<App>>,
    Json(input): Json<Settings>,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let roots = config::validate_roots(&input.roots)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
        if input.excludes.len() > 256
            || input
                .excludes
                .iter()
                .any(|e| e.is_empty() || e.len() > 1024 || e.contains('\0'))
            || input.index_interval_minutes > 43_200
        {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Invalid exclusions or interval (maximum 43200 minutes)",
            ));
        }
        let mut cfg = app.config.lock().unwrap();
        if app.index.progress.lock().unwrap().running {
            return Err(error(
                StatusCode::CONFLICT,
                "Wait for indexing to finish, or cancel it first",
            ));
        }
        let next = Config {
            password_hash: cfg.password_hash.clone(),
            roots,
            excludes: input.excludes,
            index_interval_minutes: input.index_interval_minutes,
        };
        config::save(&app.data_dir, &next).map_err(internal)?;
        *cfg = next.clone();
        app.index.start(next, app.data_dir.clone());
        Ok(Json(json!({"ok": true})))
    })
    .await
    .map_err(internal)?
}

async fn reindex(State(app): State<Arc<App>>) -> Result<Json<Value>, ApiError> {
    let cfg = app.config.lock().unwrap();
    if !app.index.start(cfg.clone(), app.data_dir.clone()) {
        return Err(error(StatusCode::CONFLICT, "Indexing is already running"));
    }
    Ok(Json(json!({"ok": true})))
}

async fn cancel_index(State(app): State<Arc<App>>) -> Json<Value> {
    app.index.cancelled.store(true, Ordering::Relaxed);
    Json(json!({"ok": true}))
}

async fn websocket(
    State(app): State<Arc<App>>,
    Extension(session): Extension<Arc<Session>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let proof = format!("csrf.{}", session.csrf);
    let valid = headers
        .get("sec-websocket-protocol")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.split(',').any(|s| s.trim() == proof));
    if !valid {
        return Err(error(StatusCode::FORBIDDEN, "Invalid terminal CSRF token"));
    }
    let permit = app.terminals.clone().try_acquire_owned().map_err(|_| {
        error(
            StatusCode::TOO_MANY_REQUESTS,
            "At most four terminals may run at once",
        )
    })?;
    Ok(ws
        .protocols(["rwebman"])
        .max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| terminal::run(socket, app, session, permit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use clap::Parser;
    use tower::ServiceExt;
    #[tokio::test]
    async fn protected_routes_reject_unauthenticated_requests_and_forged_origins() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::parse_from(["rwebman"]);
        let app = Arc::new(App {
            config: Mutex::new(Config {
                password_hash: String::new(),
                roots: vec![],
                excludes: vec![],
                index_interval_minutes: 0,
            }),
            data_dir: dir.path().into(),
            index: Index::open(dir.path().join("index.sqlite")).unwrap(),
            auth: Auth::default(),
            security: Security::new(&cli).unwrap(),
            shell: config::shell(&cli),
            auto_index: false,
            terminals: Arc::new(Semaphore::new(4)),
            searches: Arc::new(Semaphore::new(4)),
            logins: Arc::new(Semaphore::new(2)),
        });
        let router = router(app.clone());
        for path in [
            "/api/search?q=secret",
            "/api/settings",
            "/api/status",
            "/api/session",
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:8080")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("host", "attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let (token, session) = app.auth.create().unwrap();
        for (origin, csrf, expected) in [
            (
                "http://evil.example",
                session.csrf.as_str(),
                StatusCode::FORBIDDEN,
            ),
            ("http://127.0.0.1:8080", "bad", StatusCode::FORBIDDEN),
            (
                "http://127.0.0.1:8080",
                session.csrf.as_str(),
                StatusCode::OK,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/logout")
                        .header("host", "127.0.0.1:8080")
                        .header("origin", origin)
                        .header("x-csrf-token", csrf)
                        .header("cookie", format!("{}={token}", auth::COOKIE_NAME))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        assert!(session.cancelled.is_cancelled());
    }
    #[test]
    fn remote_http_requires_explicit_opt_in() {
        assert!(Security::new(&Cli::parse_from(["rwebman", "--bind", "0.0.0.0:8080"])).is_err());
        assert!(
            Security::new(&Cli::parse_from([
                "rwebman",
                "--public-url",
                "https://example.com/path"
            ]))
            .is_err()
        );
        assert!(
            Security::new(&Cli::parse_from([
                "rwebman",
                "--public-url",
                "https://example.com"
            ]))
            .unwrap()
            .secure_cookie
        );
    }
}
