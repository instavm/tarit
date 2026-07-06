use axum::{
    http::{header, HeaderMap},
    response::{Html, IntoResponse},
};

const OPENAPI_YAML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.yaml"));

const DOCS_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>InstaVM Orchestrator API</title>
  <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css"/>
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    SwaggerUIBundle({ url: "/openapi.yaml", dom_id: "#swagger-ui" });
  </script>
</body>
</html>"##;

pub async fn spec(headers: HeaderMap) -> impl IntoResponse {
    let base = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|host| format!("http://{host}"))
        .unwrap_or_else(|| "http://localhost:8080".into());
    let body = OPENAPI_YAML.replace("http://localhost:8080", &base);
    ([(header::CONTENT_TYPE, "application/yaml")], body)
}

pub async fn docs() -> Html<&'static str> {
    Html(DOCS_HTML)
}
