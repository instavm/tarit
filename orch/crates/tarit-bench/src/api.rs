use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::report::RunContext;

#[derive(Clone)]
pub struct TaritClient {
    client: Client,
    base_url: String,
    api_key: String,
}

impl TaritClient {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder().build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        })
    }

    pub async fn create_vm(&self, ctx: &RunContext) -> Result<CreateVmResponse> {
        let body = CreateVmRequest {
            memory_mib: ctx.memory_mib,
            vcpus: ctx.vcpus,
            kernel_path: ctx.kernel_path.clone(),
            rootfs_path: ctx.rootfs.clone(),
        };
        let response = self
            .client
            .post(self.url("/v1/vms"))
            .header("X-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await?;

        if response.status() != StatusCode::CREATED {
            return Err(response_error(response).await);
        }

        Ok(response.json().await?)
    }

    #[allow(dead_code)]
    pub async fn execute_async(
        &self,
        vm_id: Uuid,
        command: &str,
        timeout_ms: u64,
    ) -> Result<ExecutionResponse> {
        let body = ExecuteRequest {
            vm_id,
            command,
            timeout_ms,
        };
        let response = self
            .client
            .post(self.url("/v1/execute_async"))
            .header("X-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await?;

        if response.status() != StatusCode::ACCEPTED {
            return Err(response_error(response).await);
        }

        Ok(response.json().await?)
    }

    pub async fn execute(
        &self,
        vm_id: Uuid,
        command: &str,
        timeout_ms: u64,
    ) -> Result<ExecutionResponse> {
        let body = ExecuteRequest {
            vm_id,
            command,
            timeout_ms,
        };
        let response = self
            .client
            .post(self.url("/v1/execute"))
            .header("X-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(response_error(response).await);
        }

        Ok(response.json().await?)
    }

    #[allow(dead_code)]
    pub async fn get_execution(&self, id: Uuid) -> Result<ExecutionResponse> {
        let response = self
            .client
            .get(self.url(&format!("/v1/executions/{id}")))
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(response_error(response).await);
        }

        Ok(response.json().await?)
    }

    pub async fn delete_vm(&self, id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(self.url(&format!("/v1/vms/{id}")))
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(response_error(response).await);
        }

        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[derive(Debug, Serialize)]
struct CreateVmRequest {
    memory_mib: u64,
    vcpus: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rootfs_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateVmResponse {
    pub id: Uuid,
    pub status: String,
    pub startup_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExecuteRequest<'a> {
    vm_id: Uuid,
    command: &'a str,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct ExecutionResponse {
    pub id: Uuid,
    pub status: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

async fn response_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_else(|err| err.to_string());
    anyhow!("HTTP {status}: {body}")
}
