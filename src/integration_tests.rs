use crate::{
    auth,
    config::{self, Cli, Config},
    index::Index,
    server::{self, App, Security},
};
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tower::ServiceExt;

async fn request(
    router: &axum::Router,
    origin: &str,
    method: &str,
    uri: &str,
    cookie: &str,
    csrf: &str,
    body: Value,
) -> Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", origin.strip_prefix("http://").unwrap())
                .header("origin", origin)
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .extension(ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap()))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_index_search_real_terminal_and_logout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rwebman=debug")
        .with_test_writer()
        .try_init();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(root.join("notes_你好.txt"), "not indexed as content").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let mut cli = Cli::parse_from(["rwebman"]);
    cli.bind = address;
    // Avoid personal startup profiles in the automated terminal test.
    #[cfg(windows)]
    {
        cli.shell = Some(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/cmd.exe"),
        );
    }
    #[cfg(unix)]
    {
        cli.shell = Some("/bin/sh".into());
    }
    let app = Arc::new(App {
        config: Mutex::new(Config {
            password_hash: auth::hash_password("test-only passphrase 123").unwrap(),
            roots: config::validate_roots(std::slice::from_ref(&root)).unwrap(),
            excludes: vec![],
            index_interval_minutes: 0,
        }),
        data_dir: data.clone(),
        index: Index::open(data.join("index.sqlite3")).unwrap(),
        auth: auth::Auth::default(),
        security: Security::new(&cli).unwrap(),
        shell: config::shell(&cli),
        auto_index: false,
        terminals: Arc::new(Semaphore::new(4)),
        searches: Arc::new(Semaphore::new(4)),
        logins: Arc::new(Semaphore::new(2)),
    });
    let router = server::router(app.clone());
    let service = router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, service).await.unwrap();
    });
    let wrong = request(
        &router,
        &origin,
        "POST",
        "/api/login",
        "",
        "",
        json!({"password": "wrong"}),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let login = request(
        &router,
        &origin,
        "POST",
        "/api/login",
        "",
        "",
        json!({"password": "test-only passphrase 123"}),
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let set_cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let session = json_body(login).await;
    let csrf = session["csrf"].as_str().unwrap();
    let response = request(
        &router,
        &origin,
        "POST",
        "/api/index",
        &cookie,
        csrf,
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.index.progress.lock().unwrap().running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let response = request(
        &router,
        &origin,
        "GET",
        "/api/search?q=notes",
        &cookie,
        csrf,
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let results = json_body(response).await;
    assert_eq!(results["entries"][0]["name"], "notes_你好.txt");
    let settings = json_body(
        request(
            &router,
            &origin,
            "GET",
            "/api/settings",
            &cookie,
            csrf,
            json!(null),
        )
        .await,
    )
    .await;
    assert!(settings.get("password_hash").is_none());
    let update = json!({"roots": [root], "excludes": ["skip-me"], "index_interval_minutes": 15});
    assert_eq!(
        request(
            &router,
            &origin,
            "PUT",
            "/api/settings",
            &cookie,
            csrf,
            update
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        config::load(&data).unwrap().unwrap().index_interval_minutes,
        15
    );

    let ws_url = format!("ws://{address}/api/terminal");
    for (test_cookie, protocol, expected) in [
        (
            "",
            format!("rwebman, csrf.{csrf}"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            cookie.as_str(),
            "rwebman, csrf.invalid".into(),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let mut req = ws_url.clone().into_client_request().unwrap();
        req.headers_mut().insert("origin", origin.parse().unwrap());
        req.headers_mut()
            .insert("cookie", test_cookie.parse().unwrap());
        req.headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
        let failure = connect_async(req).await.unwrap_err();
        match failure {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), expected)
            }
            other => panic!("Unexpected error: {other}"),
        }
    }
    let mut req = ws_url.into_client_request().unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("rwebman, csrf.{csrf}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(req).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"resize","cols":100,"rows":28})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    #[cfg(windows)]
    let command = "set /a _rwm=19000+731\recho RWM_%_rwm%\r";
    #[cfg(unix)]
    let command = "_rwm=$((19000+731))\nprintf 'RWM_%s\\n' \"$_rwm\"\n";
    socket
        .send(Message::Text(
            json!({"type":"input","data":command}).to_string().into(),
        ))
        .await
        .unwrap();
    let mut output = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(message) = socket.next().await {
            match message.unwrap() {
                Message::Binary(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if text.contains("\x1b[6n") {
                        socket
                            .send(Message::Text(
                                json!({"type":"input","data":"\x1b[1;1R"})
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    output.push_str(&text);
                    if output.contains("RWM_19731") {
                        return;
                    }
                }
                Message::Ping(_) => {
                    socket.flush().await.unwrap();
                }
                other => panic!("Unexpected terminal message: {other:?}; output: {output:?}"),
            }
        }
        panic!("Terminal disconnected before command completed: {output}");
    })
    .await
    .unwrap_or_else(|_| panic!("Terminal timed out: {output}"));
    assert_eq!(
        request(
            &router,
            &origin,
            "POST",
            "/api/logout",
            &cookie,
            csrf,
            json!(null)
        )
        .await
        .status(),
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(message) = socket.next().await {
            if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    })
    .await
    .expect("Logout must close the terminal");
    tokio::time::timeout(Duration::from_secs(6), async {
        while app.terminals.available_permits() < 4 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Logout must reap the shell process");
    assert_eq!(
        request(
            &router,
            &origin,
            "GET",
            "/api/search?q=notes",
            &cookie,
            csrf,
            json!(null)
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    while app.index.progress.lock().unwrap().running {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server_task.abort();
}
