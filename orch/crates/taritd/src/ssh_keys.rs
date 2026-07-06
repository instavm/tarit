use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tarit_types::{openssh_sha256_fingerprint, OrchError, SshKeyRecord};
use uuid::Uuid;

use crate::api::{store_err, ApiError, AppState};
use crate::config::ApiIdentity;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateSshKeyRequest {
    public_key: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SshKeyResponse {
    id: Uuid,
    fingerprint: String,
    key_type: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListSshKeysResponse {
    keys: Vec<SshKeyResponse>,
}

struct ParsedPublicKey {
    key_type: String,
    blob: Vec<u8>,
}

pub(crate) async fn create_ssh_key(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<CreateSshKeyRequest>,
) -> Result<(StatusCode, Json<SshKeyResponse>), ApiError> {
    let parsed = parse_openssh_public_key(&req.public_key)?;
    let record = SshKeyRecord {
        id: Uuid::new_v4(),
        owner_key: identity.tenant,
        fingerprint: openssh_sha256_fingerprint(&parsed.blob),
        public_key: req.public_key.trim().to_string(),
        key_type: parsed.key_type,
        created_at: Utc::now(),
        is_active: true,
    };

    let store = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock".into()))?;
    store.insert_ssh_key(&record).map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(SshKeyResponse::from(&record))))
}

pub(crate) async fn list_ssh_keys(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<ListSshKeysResponse>, ApiError> {
    let store = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock".into()))?;
    let keys = store
        .list_ssh_keys(&identity.tenant)
        .map_err(store_err)?
        .iter()
        .map(SshKeyResponse::from)
        .collect();
    Ok(Json(ListSshKeysResponse { keys }))
}

pub(crate) async fn delete_ssh_key(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let store = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock".into()))?;
    store
        .delete_ssh_key(&identity.tenant, key_id)
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

impl From<&SshKeyRecord> for SshKeyResponse {
    fn from(value: &SshKeyRecord) -> Self {
        Self {
            id: value.id,
            fingerprint: value.fingerprint.clone(),
            key_type: value.key_type.clone(),
            created_at: value.created_at,
        }
    }
}

fn parse_openssh_public_key(line: &str) -> Result<ParsedPublicKey, OrchError> {
    let mut fields = line.split_whitespace();
    let key_type = fields
        .next()
        .ok_or_else(|| OrchError::BadRequest("missing SSH key type".into()))?;
    let key_blob = fields
        .next()
        .ok_or_else(|| OrchError::BadRequest("missing SSH key blob".into()))?;
    if key_type.is_empty() || key_type.contains(',') {
        return Err(OrchError::BadRequest("invalid SSH key type".into()));
    }

    let blob = decode_base64_key_blob(key_blob)?;
    let mut reader = BlobReader::new(&blob);
    let blob_type = reader.read_string_utf8()?;
    if blob_type != key_type {
        return Err(OrchError::BadRequest(format!(
            "SSH key type mismatch: line has {key_type}, blob has {blob_type}"
        )));
    }
    validate_key_body(key_type, &mut reader)?;
    if !reader.is_done() {
        return Err(OrchError::BadRequest(
            "SSH key blob has trailing bytes".into(),
        ));
    }

    Ok(ParsedPublicKey {
        key_type: key_type.to_string(),
        blob,
    })
}

fn decode_base64_key_blob(key_blob: &str) -> Result<Vec<u8>, OrchError> {
    match general_purpose::STANDARD.decode(key_blob) {
        Ok(blob) => Ok(blob),
        Err(first_err) => {
            let missing_padding = (4 - (key_blob.len() % 4)) % 4;
            if missing_padding == 0 {
                return Err(OrchError::BadRequest(format!(
                    "invalid SSH key base64: {first_err}"
                )));
            }
            let mut padded = key_blob.to_string();
            padded.extend(std::iter::repeat_n('=', missing_padding));
            general_purpose::STANDARD
                .decode(padded)
                .map_err(|e| OrchError::BadRequest(format!("invalid SSH key base64: {e}")))
        }
    }
}

fn validate_key_body(key_type: &str, reader: &mut BlobReader<'_>) -> Result<(), OrchError> {
    match key_type {
        "ssh-ed25519" => {
            let key = reader.read_string_bytes()?;
            if key.len() != 32 {
                return Err(OrchError::BadRequest(
                    "ssh-ed25519 public key must be 32 bytes".into(),
                ));
            }
        }
        "ssh-rsa" => {
            reader.read_mpint("rsa exponent")?;
            reader.read_mpint("rsa modulus")?;
        }
        "ssh-dss" => {
            reader.read_mpint("dss p")?;
            reader.read_mpint("dss q")?;
            reader.read_mpint("dss g")?;
            reader.read_mpint("dss y")?;
        }
        "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" => {
            let curve = reader.read_string_utf8()?;
            let expected = key_type.trim_start_matches("ecdsa-sha2-");
            if curve != expected {
                return Err(OrchError::BadRequest(format!(
                    "ECDSA curve mismatch: expected {expected}, got {curve}"
                )));
            }
            let point = reader.read_string_bytes()?;
            if point.is_empty() {
                return Err(OrchError::BadRequest("ECDSA public point is empty".into()));
            }
        }
        "sk-ssh-ed25519@openssh.com" => {
            let key = reader.read_string_bytes()?;
            if key.len() != 32 {
                return Err(OrchError::BadRequest(
                    "sk-ssh-ed25519 public key must be 32 bytes".into(),
                ));
            }
            let application = reader.read_string_utf8()?;
            if application.is_empty() {
                return Err(OrchError::BadRequest(
                    "security key application is empty".into(),
                ));
            }
        }
        "sk-ecdsa-sha2-nistp256@openssh.com" => {
            let curve = reader.read_string_utf8()?;
            if curve != "nistp256" {
                return Err(OrchError::BadRequest(format!(
                    "security key ECDSA curve mismatch: {curve}"
                )));
            }
            let point = reader.read_string_bytes()?;
            if point.is_empty() {
                return Err(OrchError::BadRequest(
                    "security key ECDSA public point is empty".into(),
                ));
            }
            let application = reader.read_string_utf8()?;
            if application.is_empty() {
                return Err(OrchError::BadRequest(
                    "security key application is empty".into(),
                ));
            }
        }
        _ => {
            return Err(OrchError::BadRequest(format!(
                "unsupported SSH key type: {key_type}"
            )));
        }
    }
    Ok(())
}

struct BlobReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BlobReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn is_done(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn read_string_utf8(&mut self) -> Result<String, OrchError> {
        let bytes = self.read_string_bytes()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| OrchError::BadRequest("SSH key blob contains non-UTF-8 string".into()))
    }

    fn read_string_bytes(&mut self) -> Result<&'a [u8], OrchError> {
        if self.bytes.len().saturating_sub(self.pos) < 4 {
            return Err(OrchError::BadRequest("truncated SSH key blob".into()));
        }
        let len = u32::from_be_bytes([
            self.bytes[self.pos],
            self.bytes[self.pos + 1],
            self.bytes[self.pos + 2],
            self.bytes[self.pos + 3],
        ]) as usize;
        self.pos += 4;
        if self.bytes.len().saturating_sub(self.pos) < len {
            return Err(OrchError::BadRequest("truncated SSH key blob".into()));
        }
        let out = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn read_mpint(&mut self, name: &str) -> Result<&'a [u8], OrchError> {
        let value = self.read_string_bytes()?;
        if value.is_empty() {
            return Err(OrchError::BadRequest(format!("{name} is empty")));
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g test@example";
    const ED25519_FINGERPRINT: &str = "SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4";

    #[test]
    fn computes_openssh_sha256_fingerprint() {
        let parsed = parse_openssh_public_key(ED25519_KEY).unwrap();

        assert_eq!(parsed.key_type, "ssh-ed25519");
        assert_eq!(
            openssh_sha256_fingerprint(&parsed.blob),
            ED25519_FINGERPRINT
        );
    }

    #[test]
    fn rejects_mismatched_key_type() {
        let bad = ED25519_KEY.replacen("ssh-ed25519", "ssh-rsa", 1);

        assert!(matches!(
            parse_openssh_public_key(&bad),
            Err(OrchError::BadRequest(_))
        ));
    }
}
