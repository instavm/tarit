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

#[cfg(test)]
mod tests {
    use super::OPENAPI_YAML;

    const TYPES_SOURCE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tarit-types/src/lib.rs"
    ));

    fn recorded_audit_actions() -> Vec<&'static str> {
        let module = TYPES_SOURCE
            .split("pub mod audit_action {")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("audit_action module");
        module
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub const "))
            .map(|decl| decl.split('"').nth(1).expect("string audit action"))
            .collect()
    }

    fn documented_audit_actions() -> Vec<&'static str> {
        let action = OPENAPI_YAML
            .split("\n    AuditEvent:\n")
            .nth(1)
            .and_then(|schema| schema.split("\n        action:\n").nth(1))
            .expect("AuditEvent.action schema");
        action
            .lines()
            .find_map(|line| line.trim().strip_prefix("enum: ["))
            .and_then(|values| values.strip_suffix(']'))
            .expect("single-line AuditEvent.action enum")
            .split(',')
            .map(str::trim)
            .collect()
    }

    #[test]
    fn audit_event_action_enum_matches_recorded_actions() {
        let mut recorded = recorded_audit_actions();
        assert!(recorded.contains(&tarit_types::audit_action::CREATE));
        assert!(recorded.contains(&tarit_types::audit_action::SET_BALLOON));
        let mut documented = documented_audit_actions();
        recorded.sort_unstable();
        documented.sort_unstable();
        assert_eq!(documented, recorded);
    }
}
