//! Common HTTP hop-header rules; proxy-specific trust filters stay with callers.
use axum::http::{
    header::{HeaderName, CONNECTION},
    HeaderMap,
};
use std::{collections::HashSet, str::FromStr};

pub(crate) fn connection_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_str(value.trim()).ok())
        .collect()
}

pub(crate) fn is_hop_by_hop(name: &HeaderName, connection_headers: &HashSet<HeaderName>) -> bool {
    connection_headers.contains(name)
        || matches!(
            name.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-connection"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_connection_values_nominate_case_insensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.append(CONNECTION, "keep-alive, X-Private".parse().unwrap());
        headers.append(
            CONNECTION,
            "Upgrade, x-second, invalid token".parse().unwrap(),
        );
        let nominated = connection_headers(&headers);
        for name in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "x-private",
            "x-second",
        ] {
            assert!(
                is_hop_by_hop(&HeaderName::from_static(name), &nominated),
                "{name}"
            );
        }
        for name in [
            "content-type",
            "host",
            "forwarded",
            "x-api-key",
            "sec-websocket-protocol",
        ] {
            assert!(
                !is_hop_by_hop(&HeaderName::from_static(name), &nominated),
                "{name}"
            );
        }
        assert_eq!(nominated.len(), 4);
    }
}
