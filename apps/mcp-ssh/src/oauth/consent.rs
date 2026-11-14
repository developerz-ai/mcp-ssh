//! The `/authorize` consent page: a minimal login form the operator submits to
//! approve a client. A static template, no engine — the only dynamic values are
//! HTML-escaped. Served un-cacheable and un-frameable, so it can't be replayed from
//! a cache or clickjacked under a hostile page.
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>mcp-ssh login</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 26rem; margin: 3rem auto; padding: 0 1rem; color: #222; background: #fff; }
label { display: block; margin-top: 1rem; }
input[type=text], input[type=password] { width: 100%; padding: .5rem; box-sizing: border-box; }
button { margin-top: 1.25rem; padding: .5rem 1.25rem; }
.client { font-family: monospace; word-break: break-all; }
.error { color: #b00020; }
</style>
</head>
<body>
<h1>Authorize client</h1>
<p>A client is asking for shell access to this host. After you log in, it is sent to:</p>
<p class="client">{{CLIENT}}</p>
{{ERROR}}
<form method="post" action="authorize">
<input type="hidden" name="csrf_token" value="{{CSRF}}">
<label>Username <input type="text" name="username" autocomplete="username" required autofocus></label>
<label>Password <input type="password" name="password" autocomplete="current-password" required></label>
<button type="submit">Log in and authorize</button>
</form>
</body>
</html>
"#;

/// Render the form for one pending request. `nonce` is the single-use token the
/// form posts back; `destination` is where the code will go (shown so the
/// operator can spot a client they didn't start).
pub fn page(status: StatusCode, nonce: &str, destination: &str, error: Option<&str>) -> Response {
    let error = error
        .map(|e| format!("<p class=\"error\">{}</p>", escape(e)))
        .unwrap_or_default();
    let html = PAGE
        .replace("{{CLIENT}}", &escape(destination))
        .replace("{{ERROR}}", &error)
        .replace("{{CSRF}}", &escape(nonce));
    (status, page_headers(), html).into_response()
}

/// A plain page for a consent that can't continue (expired, replayed, forged).
pub fn refused(status: StatusCode, message: &str) -> Response {
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>mcp-ssh login</title><p>{}</p>",
        escape(message)
    );
    (status, page_headers(), html).into_response()
}

fn page_headers() -> [(header::HeaderName, &'static str); 5] {
    [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        // No `form-action`: browsers apply it to the 302 that carries the code to
        // the client's own origin, which would break the flow.
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
    ]
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_page_escapes_every_dynamic_value_and_is_uncacheable_and_unframeable() {
        let resp = page(
            StatusCode::OK,
            "nonce\"><x>",
            "https://<script>.example/cb",
            Some("<b>bad</b>"),
        );
        let h = resp.headers();
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert!(
            h[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let html = body(resp).await;
        assert!(!html.contains("<script>") && !html.contains("<b>") && !html.contains("<x>"));
        assert!(html.contains("name=\"csrf_token\" value=\"nonce&quot;&gt;&lt;x&gt;\""));
    }
}
