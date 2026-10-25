//! Serves the single-page console from disk.
//!
//! ## Why not `include_bytes!`
//!
//! The built single-page app is produced by npm into a directory that does not
//! exist in a clean checkout. Embedding it would make `cargo build` fail until
//! someone runs the front-end build, which is a bad trade for a project whose
//! deployment story is "one self-contained binary": the binary must build from
//! source with nothing but a Rust toolchain.
//!
//! The assets are therefore located at runtime, and when they are absent the
//! gateway serves a self-describing placeholder. That keeps three things true at
//! once: a clean checkout builds, a deployment needs no separate web server, and
//! a missing front-end build is obvious rather than mysterious.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use percent_encoding::percent_decode_str;

/// Overrides the directory the front end is served from.
pub const WEB_DIR_ENV: &str = "CUSTOMS_WEB_DIR";

/// Directories searched in order for a built `index.html`.
///
/// The list covers running from the source tree, from a release directory, and
/// from the installed location on FreeBSD.
pub const DEFAULT_WEB_DIRS: [&str; 6] = [
    "web/dist",
    "web",
    "../web/dist",
    "../../web/dist",
    "/usr/local/share/customapi/web/dist",
    "/usr/local/share/customapi/web",
];

/// Finds a directory holding a built `index.html`.
pub fn locate_web_dir(base: &Path) -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var(WEB_DIR_ENV) {
        if !override_dir.is_empty() {
            let candidate = PathBuf::from(&override_dir);
            if file_exists(&candidate.join("index.html")) {
                return Some(candidate);
            }
        }
    }
    for dir in DEFAULT_WEB_DIRS {
        let candidate = PathBuf::from(dir);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            base.join(candidate)
        };
        if file_exists(&candidate.join("index.html")) {
            return Some(candidate);
        }
    }
    None
}

/// Reports whether a path exists and is a regular file.
fn file_exists(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| !m.is_dir())
        .unwrap_or(false)
}

/// Serves the console, or the placeholder when no build is present.
///
/// `path` is the request path with any `/ui/` prefix already removed.
pub(crate) async fn serve_spa(dir: Option<&Path>, path: &str) -> Response {
    let Some(dir) = dir else {
        return serve_web_placeholder();
    };
    let decoded = percent_decode_str(path.trim_start_matches('/'))
        .decode_utf8_lossy()
        .into_owned();
    if decoded.is_empty() {
        return serve_index_or_placeholder(dir).await;
    }
    // Go's `http.Dir` rejects a path containing "..", and the fallback for a
    // rejected path is the application shell, not an error page.
    let traversal = decoded
        .split(['/', '\\'])
        .any(|part| part == ".." || part == ".");
    if traversal {
        return serve_index_or_placeholder(dir).await;
    }
    let candidate = dir.join(&decoded);
    match tokio::fs::metadata(&candidate).await {
        Ok(meta) if meta.is_file() => match tokio::fs::read(&candidate).await {
            Ok(bytes) => {
                let content_type = content_type_for(&candidate);
                let body = Body::from(bytes);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, content_type)
                    .body(body)
                    .unwrap_or_else(|_| Response::new(Body::empty()))
            }
            Err(_) => serve_index_or_placeholder(dir).await,
        },
        Ok(_) => {
            // A directory request: Go's file server answers with its index.html.
            let index = candidate.join("index.html");
            if file_exists(&index) {
                serve_file(&index).await
            } else {
                serve_index_or_placeholder(dir).await
            }
        }
        // Not a file: treat it as an application route and hand back the shell so
        // a refresh on a deep link works.
        Err(_) => serve_index_or_placeholder(dir).await,
    }
}

/// Returns `index.html`, falling back to the placeholder if it has disappeared
/// since the directory was located.
async fn serve_index_or_placeholder(dir: &Path) -> Response {
    let index = dir.join("index.html");
    if !file_exists(&index) {
        return serve_web_placeholder();
    }
    serve_file(&index).await
}

/// Serves one file with the shell's cache policy.
async fn serve_file(path: &Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
            // The shell must not be cached: it references hashed asset names that
            // change on every build, and a stale shell 404s its own assets.
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| Response::new(Body::empty())),
        Err(_) => serve_web_placeholder(),
    }
}

/// Explains that the UI has not been built.
///
/// It deliberately documents the admin API, because the API is fully usable
/// without the UI and an operator who lands here should not be blocked.
fn serve_web_placeholder() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(PLACEHOLDER_HTML))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Maps a file extension onto the content type browsers need.
fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The placeholder shown when the front end has not been built.
pub const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Custom-API 海关网关</title>
<style>
  body{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;background:#0f1115;color:#e6e6e6;
       margin:0;display:flex;align-items:center;justify-content:center;min-height:100vh}
  .card{max-width:680px;padding:2.5rem;background:#171a21;border:1px solid #2a2f3a;border-radius:12px}
  h1{margin:0 0 .75rem;font-size:1.35rem}
  p{line-height:1.75;color:#a8b0bd;margin:.6rem 0}
  code{background:#0f1115;padding:.15rem .4rem;border-radius:4px;color:#7dd3fc;font-size:.9em}
  ul{line-height:1.9;color:#a8b0bd}
  .ok{color:#4ade80}.warn{color:#fbbf24}
</style>
</head>
<body>
<div class="card">
  <h1>Custom-API 海关网关</h1>
  <p class="ok">网关正在运行，管理 API 完全可用。</p>
  <p class="warn">前端界面尚未构建，因此这里显示的是占位页。</p>
  <p>构建前端：<code>cd src/customapi/web &amp;&amp; npm ci &amp;&amp; npm run build</code></p>
  <p>或把已构建的目录指给网关：<code>CUSTOMS_WEB_DIR=/path/to/dist</code></p>
  <p>在构建之前，可以直接使用这些接口：</p>
  <ul>
    <li><code>GET /admin/stats</code> — 运行状态与计数</li>
    <li><code>GET /admin/providers</code> — 提供商、密钥与健康度</li>
    <li><code>GET /admin/models</code> — 模型目录（发现 + 手写）</li>
    <li><code>POST /admin/models/refresh</code> — 立即重新发现模型</li>
    <li><code>GET /admin/usage</code> — 按渠道/模型/用户聚合</li>
    <li><code>GET /admin/audit</code> — 审计记录</li>
    <li><code>GET /admin/audit/verify</code> — 校验哈希链完整性</li>
    <li><code>GET /v1/models</code> — 用户可见的模型列表（需 API Key）</li>
  </ul>
</div>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_keeps_its_chinese_content() {
        assert!(PLACEHOLDER_HTML.contains("前端界面尚未构建"));
        assert!(PLACEHOLDER_HTML.contains("CUSTOMS_WEB_DIR=/path/to/dist"));
        assert!(PLACEHOLDER_HTML.starts_with("<!doctype html>"));
    }

    #[test]
    fn content_types_cover_the_bundle() {
        assert_eq!(
            content_type_for(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("assets/index-abc.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(
            content_type_for(Path::new("unknown.bin")),
            "application/octet-stream"
        );
    }
}

/// The SPA file server: paths, fallbacks and the placeholder.
#[cfg(test)]
mod spa_tests {
    use super::*;
    use crate::audit::testutil::TempDir;
    use http_body_util::BodyExt;

    /// Creates `<dir>/index.html` and returns the directory.
    fn site(dir: &TempDir, name: &str, shell: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).expect("create site dir");
        std::fs::write(root.join("index.html"), shell).expect("write index");
        root
    }

    async fn body_text(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn content_type(response: &Response) -> String {
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn locate_finds_a_built_console_below_the_base_directory() {
        let dir = TempDir::new();
        let base = dir.join("release");
        std::fs::create_dir_all(base.join("web/dist")).expect("dirs");
        std::fs::write(base.join("web/dist/index.html"), "shell").expect("index");
        assert_eq!(locate_web_dir(&base), Some(base.join("web/dist")));
    }

    #[test]
    fn locate_returns_none_without_a_build() {
        let dir = TempDir::new();
        assert_eq!(locate_web_dir(&dir.join("empty")), None);
    }

    #[tokio::test]
    async fn serves_an_asset_with_its_content_type() {
        let dir = TempDir::new();
        let root = site(&dir, "assets", "shell");
        std::fs::write(root.join("app.js"), "console.log(1)").expect("asset");

        let response = serve_spa(Some(&root), "/app.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), "text/javascript; charset=utf-8");
        assert_eq!(body_text(response).await, "console.log(1)");
    }

    #[tokio::test]
    async fn unknown_extensions_are_served_as_octet_stream() {
        let dir = TempDir::new();
        let root = site(&dir, "binary", "shell");
        std::fs::write(root.join("data.bin"), [0u8, 1, 2]).expect("asset");

        let response = serve_spa(Some(&root), "/data.bin").await;
        assert_eq!(content_type(&response), "application/octet-stream");
    }

    #[tokio::test]
    async fn a_deep_link_falls_back_to_the_shell() {
        let dir = TempDir::new();
        let root = site(&dir, "spa", "the-shell");

        let response = serve_spa(Some(&root), "/providers").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), "text/html; charset=utf-8");
        // The shell must never be cached: it references hashed asset names.
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
        assert_eq!(body_text(response).await, "the-shell");
    }

    #[tokio::test]
    async fn path_traversal_is_answered_with_the_shell() {
        let dir = TempDir::new();
        let root = site(&dir, "spa", "the-shell");
        std::fs::write(dir.join("secret.txt"), "do-not-serve").expect("secret");

        for path in ["/../secret.txt", "/./secret.txt", "/a/../../secret.txt"] {
            let response = serve_spa(Some(&root), path).await;
            assert_eq!(response.status(), StatusCode::OK, "path {path}");
            assert_eq!(
                body_text(response).await,
                "the-shell",
                "path {path} must not escape the web root"
            );
        }
    }

    #[tokio::test]
    async fn a_directory_request_serves_its_own_index() {
        let dir = TempDir::new();
        let root = site(&dir, "spa", "the-shell");
        std::fs::create_dir_all(root.join("docs")).expect("dir");
        std::fs::write(root.join("docs/index.html"), "docs-index").expect("index");

        let response = serve_spa(Some(&root), "/docs/").await;
        assert_eq!(body_text(response).await, "docs-index");
    }

    #[tokio::test]
    async fn a_vanished_index_falls_back_to_the_placeholder() {
        let dir = TempDir::new();
        let root = site(&dir, "spa", "the-shell");
        std::fs::remove_file(root.join("index.html")).expect("remove");

        let response = serve_spa(Some(&root), "/providers").await;
        let body = body_text(response).await;
        assert!(body.contains("CUSTOMS_WEB_DIR"), "placeholder expected");
    }

    #[tokio::test]
    async fn without_a_build_the_placeholder_explains_how_to_build_one() {
        let response = serve_spa(None, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(body.contains("npm run build"), "{body}");
        assert_eq!(
            content_type(&serve_spa(None, "/").await),
            "text/html; charset=utf-8"
        );
    }
}
