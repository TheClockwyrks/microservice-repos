//! Azure Blob Storage over its REST API, signed by hand.
//!
//! The bundles live in one private Blob container. The service needs a handful
//! of operations on it: write an object whole (`Put Blob`, or `Put Block` and
//! `Put Block List` for an object larger than one [piece](PIECE_SIZE)), read one
//! as a stream (`Get Blob`), delete one, list one blob to prove the container,
//! and create the container on an emulator. Signing these few requests directly
//! over `reqwest` costs far less than vendoring the Azure SDK, and is what the
//! artifacts service does over its own container.
//!
//! A request authenticates one of two ways ([`Credential`]):
//!
//! - **Workload identity**, in a cluster: an Entra access token for Azure
//!   Storage, minted from the pod's projected `ServiceAccount` token and cached
//!   ([`WorkloadIdentity`]), sent as `Authorization: Bearer`.
//! - **Shared Key**, against Azurite locally: the request is signed with the
//!   account key (see `blob.sign.rs`).
//!
//! The service version every request names is pinned ([`API_VERSION`]), so the
//! store's behaviour never shifts under the service.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use reqwest::Method;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::io::AsyncReadExt as _;

#[path = "blob.identity.rs"]
mod identity;
#[path = "blob.sign.rs"]
mod sign;

pub use identity::WorkloadIdentity;

/// The Blob service version every request is sent under (`x-ms-version`).
/// Azurite accepts it from release 3.29.
pub const API_VERSION: &str = "2023-11-03";

/// The size of one staged block of a large upload, and the largest object
/// written in one `Put Blob`. At 8 MiB a bundle at the upload cap stays far
/// below the store's 50,000-block limit, and the one piece the writer holds is
/// a modest allocation.
pub const PIECE_SIZE: usize = 8 * 1024 * 1024;

/// The HTTP date format of `x-ms-date`.
const HTTP_DATE: &[FormatItem<'_>] = format_description!(
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
);

/// A Blob request failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// A workload-identity variable the webhook projects is unset.
    #[error(
        "the workload identity variable `{0}` is unset (the pod needs the \
         `azure.workload.identity/use` label and a federated ServiceAccount)"
    )]
    MissingEnv(&'static str),
    /// The projected `ServiceAccount` token could not be read.
    #[error("reading the federated token file `{path}`: {source}")]
    TokenFile {
        /// The file (`AZURE_FEDERATED_TOKEN_FILE`).
        path: String,
        /// Why it could not be read.
        source: std::io::Error,
    },
    /// The Entra token exchange failed.
    #[error("the workload identity token: {0}")]
    Token(String),
    /// The request could not be sent, or its response not read.
    #[error("Blob {operation} `{target}`: {source}")]
    Transport {
        /// The operation (`put`, `get`, `list`, …).
        operation: &'static str,
        /// The container or blob it named.
        target: String,
        /// The transport failure.
        source: reqwest::Error,
    },
    /// The store answered a status the operation does not accept.
    #[error("Blob {operation} `{target}` answered {status} {code}: {detail}")]
    Status {
        /// The operation.
        operation: &'static str,
        /// The container or blob it named.
        target: String,
        /// The HTTP status.
        status: u16,
        /// The store's error code (`x-ms-error-code`), empty when absent.
        code: String,
        /// The response body, which carries the store's explanation.
        detail: String,
    },
    /// The local file being written or read failed.
    #[error("the local file for Blob `{target}`: {source}")]
    File {
        /// The blob being written or read.
        target: String,
        /// The file's failure.
        source: std::io::Error,
    },
}

/// How the service authenticates to its storage account, as configured.
#[derive(Clone, PartialEq, Eq)]
pub enum BlobAuth {
    /// Workload identity: the pod's federated managed identity.
    WorkloadIdentity,
    /// Shared Key with the account's decoded key (Azurite locally).
    SharedKey(Vec<u8>),
}

impl std::fmt::Debug for BlobAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkloadIdentity => f.write_str("WorkloadIdentity"),
            Self::SharedKey(_) => f.write_str("SharedKey(<redacted>)"),
        }
    }
}

/// The storage account and private container the bundles are written to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobConfig {
    /// The storage account (`TCAB_REPOS_BLOB_ACCOUNT`).
    pub account: String,
    /// The private bundle container (`TCAB_REPOS_BLOB_CONTAINER`).
    pub container: String,
    /// The account's Blob endpoint, without a trailing slash: the public
    /// `https://<account>.blob.core.windows.net`, or a path-style endpoint such
    /// as Azurite's `http://azurite:10000/devstoreaccount1`
    /// (`TCAB_REPOS_BLOB_ENDPOINT`).
    pub endpoint: String,
    /// How requests authenticate.
    pub auth: BlobAuth,
}

impl BlobConfig {
    /// The endpoint of `account` in the Azure public cloud.
    pub fn default_endpoint(account: &str) -> String {
        format!("https://{account}.blob.core.windows.net")
    }
}

/// The credential a request is authenticated with.
#[derive(Debug, Clone)]
pub enum Credential {
    /// An Entra token from workload identity, sent as a bearer token.
    WorkloadIdentity(Arc<WorkloadIdentity>),
    /// Shared Key signing with the account's decoded key.
    SharedKey(Arc<Vec<u8>>),
}

/// A blob's body as it arrives from the store.
pub type BlobBody = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// A blob read from the store: its length and its body.
pub struct BlobStream {
    /// The blob's length in bytes, from `Content-Length`.
    pub length: Option<u64>,
    /// The body, streamed as the store sends it.
    pub body: BlobBody,
}

impl std::fmt::Debug for BlobStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStream")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// A client of one Blob container.
#[derive(Debug, Clone)]
pub struct BlobClient {
    account: String,
    endpoint: String,
    /// The endpoint's own path (`/devstoreaccount1` for a path-style endpoint,
    /// empty for the public cloud's), which every request path starts with.
    endpoint_path: String,
    container: String,
    credential: Credential,
    http: reqwest::Client,
}

/// A header a request is sent and signed with.
type Header = (String, String);

impl BlobClient {
    /// A client of the configured container. Workload identity reads the
    /// variables the webhook projects from the process environment.
    pub fn new(config: &BlobConfig, http: reqwest::Client) -> Result<Self, BlobError> {
        let credential = match &config.auth {
            BlobAuth::SharedKey(key) => Credential::SharedKey(Arc::new(key.clone())),
            BlobAuth::WorkloadIdentity => Credential::WorkloadIdentity(Arc::new(
                WorkloadIdentity::from_lookup(|key| std::env::var(key).ok(), http.clone())?,
            )),
        };
        Ok(Self::with_credential(
            &config.account,
            &config.endpoint,
            &config.container,
            credential,
            http,
        ))
    }

    /// A client of `container` on `account` at `endpoint`, with `credential`.
    pub fn with_credential(
        account: &str,
        endpoint: &str,
        container: &str,
        credential: Credential,
        http: reqwest::Client,
    ) -> Self {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let endpoint_path = endpoint
            .split_once("://")
            .map_or(endpoint.as_str(), |(_, rest)| rest)
            .split_once('/')
            .map(|(_, path)| format!("/{path}"))
            .unwrap_or_default();
        Self {
            account: account.to_string(),
            endpoint,
            endpoint_path,
            container: container.to_string(),
            credential,
            http,
        }
    }

    /// The container this client addresses.
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Create the container. `Ok(true)` when it was created, `Ok(false)` when it
    /// already existed. Only the local stack creates its container; in a cluster
    /// it is provisioned with the account.
    #[tracing::instrument(name = "blob.create_container", skip(self), fields(blob.container = %self.container), err)]
    pub async fn create_container(&self) -> Result<bool, BlobError> {
        let response = self
            .send(
                "create-container",
                Method::PUT,
                None,
                &[("restype", "container".to_string())],
                Vec::new(),
                Bytes::new(),
            )
            .await?;
        match response.status().as_u16() {
            201 => Ok(true),
            409 => Ok(false),
            _ => Err(status_error("create-container", &self.container, response).await),
        }
    }

    /// List at most one blob of the container, which proves the credential is
    /// accepted and the container exists.
    #[tracing::instrument(name = "blob.probe", skip(self), fields(blob.container = %self.container), err)]
    pub async fn probe(&self) -> Result<(), BlobError> {
        let query = [
            ("restype", "container".to_string()),
            ("comp", "list".to_string()),
            ("maxresults", "1".to_string()),
        ];
        let response = self
            .send("list", Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        expect(response, 200, "list", &self.container).await
    }

    /// Write `body` as blob `key` in one `Put Blob`, replacing any blob of that
    /// name.
    #[tracing::instrument(name = "blob.put", skip(self, body), fields(blob.key = %key, blob.bytes = body.len()), err)]
    pub async fn put(&self, key: &str, body: Bytes, content_type: &str) -> Result<(), BlobError> {
        let headers = vec![
            (
                "x-ms-blob-content-type".to_string(),
                content_type.to_string(),
            ),
            ("x-ms-blob-type".to_string(), "BlockBlob".to_string()),
        ];
        let response = self
            .send("put", Method::PUT, Some(key), &[], headers, body)
            .await?;
        expect(response, 201, "put", key).await
    }

    /// Stage one block of blob `key` (`Put Block`).
    #[tracing::instrument(name = "blob.put_block", skip(self, body), fields(blob.key = %key, blob.bytes = body.len()), err)]
    pub async fn put_block(&self, key: &str, block_id: &str, body: Bytes) -> Result<(), BlobError> {
        let query = [
            ("comp", "block".to_string()),
            ("blockid", block_id.to_string()),
        ];
        let response = self
            .send(
                "put-block",
                Method::PUT,
                Some(key),
                &query,
                Vec::new(),
                body,
            )
            .await?;
        expect(response, 201, "put-block", key).await
    }

    /// Commit the staged blocks `ids`, in order, as blob `key` (`Put Block
    /// List`). The blob appears, or is replaced, whole at this moment.
    #[tracing::instrument(name = "blob.put_block_list", skip(self, ids), fields(blob.key = %key, blob.blocks = ids.len()), err)]
    pub async fn put_block_list(
        &self,
        key: &str,
        ids: &[String],
        content_type: &str,
    ) -> Result<(), BlobError> {
        let mut body = String::from(r#"<?xml version="1.0" encoding="utf-8"?><BlockList>"#);
        for id in ids {
            // A block id is base64, which holds nothing XML must escape.
            body.push_str("<Latest>");
            body.push_str(id);
            body.push_str("</Latest>");
        }
        body.push_str("</BlockList>");
        let headers = vec![
            (
                "x-ms-blob-content-type".to_string(),
                content_type.to_string(),
            ),
            (
                "content-type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            ),
        ];
        let response = self
            .send(
                "put-block-list",
                Method::PUT,
                Some(key),
                &[("comp", "blocklist".to_string())],
                headers,
                Bytes::from(body),
            )
            .await?;
        expect(response, 201, "put-block-list", key).await
    }

    /// Write the file at `path` as blob `key`, holding at most one
    /// [piece](PIECE_SIZE) of it: a file of one piece or less in one `Put Blob`,
    /// a larger one staged block by block and committed whole, so a reader sees
    /// the earlier blob or the new one and never a part. Returns the bytes
    /// written.
    #[tracing::instrument(name = "blob.upload_file", skip(self, path), fields(blob.key = %key), err)]
    pub async fn upload_file(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
    ) -> Result<u64, BlobError> {
        let file_error = |source| BlobError::File {
            target: key.to_string(),
            source,
        };
        let mut file = tokio::fs::File::open(path).await.map_err(file_error)?;
        let length = file.metadata().await.map_err(file_error)?.len();
        if length <= PIECE_SIZE as u64 {
            let mut body = Vec::with_capacity(length as usize);
            file.read_to_end(&mut body).await.map_err(file_error)?;
            let written = body.len() as u64;
            self.put(key, Bytes::from(body), content_type).await?;
            return Ok(written);
        }
        let mut ids = Vec::new();
        let mut written = 0u64;
        loop {
            let mut piece = Vec::with_capacity(PIECE_SIZE);
            (&mut file)
                .take(PIECE_SIZE as u64)
                .read_to_end(&mut piece)
                .await
                .map_err(file_error)?;
            if piece.is_empty() {
                break;
            }
            let id = block_id(ids.len());
            written += piece.len() as u64;
            self.put_block(key, &id, Bytes::from(piece)).await?;
            ids.push(id);
        }
        self.put_block_list(key, &ids, content_type).await?;
        Ok(written)
    }

    /// Read blob `key` as a stream. `Ok(None)` when there is no such blob.
    #[tracing::instrument(name = "blob.get", skip(self), fields(blob.key = %key), err)]
    pub async fn get(&self, key: &str) -> Result<Option<BlobStream>, BlobError> {
        let response = self
            .send("get", Method::GET, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match response.status().as_u16() {
            200 => Ok(Some(BlobStream {
                length: response.content_length(),
                body: Box::pin(
                    response
                        .bytes_stream()
                        .map(|chunk| chunk.map_err(std::io::Error::other)),
                ),
            })),
            404 => Ok(None),
            _ => Err(status_error("get", key, response).await),
        }
    }

    /// Read blob `key` into the file at `path`. `Ok(false)` when there is no
    /// such blob, and the file is then not written.
    pub async fn download_file(&self, key: &str, path: &Path) -> Result<bool, BlobError> {
        let Some(mut blob) = self.get(key).await? else {
            return Ok(false);
        };
        let file_error = |source| BlobError::File {
            target: key.to_string(),
            source,
        };
        let mut file = tokio::fs::File::create(path).await.map_err(file_error)?;
        while let Some(chunk) = blob.body.next().await {
            let chunk = chunk.map_err(file_error)?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(file_error)?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(file_error)?;
        Ok(true)
    }

    /// Delete blob `key` and any snapshots of it. `Ok(true)` when it existed,
    /// `Ok(false)` when there was nothing to delete.
    #[tracing::instrument(name = "blob.delete", skip(self), fields(blob.key = %key), err)]
    pub async fn delete(&self, key: &str) -> Result<bool, BlobError> {
        let headers = vec![("x-ms-delete-snapshots".to_string(), "include".to_string())];
        let response = self
            .send(
                "delete",
                Method::DELETE,
                Some(key),
                &[],
                headers,
                Bytes::new(),
            )
            .await?;
        match response.status().as_u16() {
            202 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error("delete", key, response).await),
        }
    }

    /// Send one authenticated request against the container, or against blob
    /// `key` in it.
    async fn send(
        &self,
        operation: &'static str,
        method: Method,
        key: Option<&str>,
        query: &[(&str, String)],
        mut headers: Vec<Header>,
        body: Bytes,
    ) -> Result<reqwest::Response, BlobError> {
        let mut path = format!(
            "{}/{}",
            self.endpoint_path,
            sign::encode(&self.container, false)
        );
        if let Some(key) = key {
            path.push('/');
            path.push_str(&sign::encode(key, true));
        }
        let target = key.unwrap_or(&self.container).to_string();

        headers.push((
            "x-ms-date".to_string(),
            OffsetDateTime::now_utc()
                .format(HTTP_DATE)
                .unwrap_or_default(),
        ));
        headers.push(("x-ms-version".to_string(), API_VERSION.to_string()));
        if !body.is_empty() || method == Method::PUT {
            headers.push(("content-length".to_string(), body.len().to_string()));
        }

        let authorization = match &self.credential {
            Credential::SharedKey(secret) => {
                let query: Vec<(String, String)> = query
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), value.clone()))
                    .collect();
                let string_to_sign = sign::string_to_sign(&sign::SignedRequest {
                    method: method.as_str(),
                    headers: &headers,
                    account: &self.account,
                    path: &path,
                    query: &query,
                });
                format!(
                    "SharedKey {}:{}",
                    self.account,
                    sign::signature(secret, &string_to_sign)
                )
            }
            Credential::WorkloadIdentity(identity) => format!("Bearer {}", identity.token().await?),
        };

        let origin = self
            .endpoint
            .strip_suffix(&self.endpoint_path)
            .unwrap_or(&self.endpoint);
        let mut url = format!("{origin}{path}");
        if !query.is_empty() {
            let encoded: Vec<String> = query
                .iter()
                .map(|(name, value)| {
                    format!(
                        "{}={}",
                        sign::encode(name, false),
                        sign::encode(value, false)
                    )
                })
                .collect();
            url.push('?');
            url.push_str(&encoded.join("&"));
        }

        // The current span's trace context joins the request to the caller's
        // trace; it is not signed, so the store ignores it.
        let mut trace_headers = http::HeaderMap::new();
        test_cabinet_telemetry::propagation::inject_current_context(&mut trace_headers);

        let mut request = self
            .http
            .request(method, &url)
            .header("authorization", authorization)
            .headers(trace_headers);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|source| BlobError::Transport {
                operation,
                target: target.clone(),
                source,
            })?;
        if response.status().as_u16() == 401
            && let Credential::WorkloadIdentity(identity) = &self.credential
        {
            // A token the store refuses is not reused: the next request mints one.
            identity.invalidate().await;
        }
        Ok(response)
    }
}

/// The id of the `index`th block of a blob: base64 of a fixed-width number, so
/// every id of the blob has the same length as the store requires.
fn block_id(index: usize) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("{index:010}"))
}

/// Accept `response` when it has status `want`, else turn it into an error.
async fn expect(
    response: reqwest::Response,
    want: u16,
    operation: &'static str,
    target: &str,
) -> Result<(), BlobError> {
    if response.status().as_u16() == want {
        Ok(())
    } else {
        Err(status_error(operation, target, response).await)
    }
}

/// The error for a response whose status the operation does not accept.
async fn status_error(
    operation: &'static str,
    target: &str,
    response: reqwest::Response,
) -> BlobError {
    let status = response.status().as_u16();
    let code = response
        .headers()
        .get("x-ms-error-code")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let detail = response.text().await.unwrap_or_default();
    BlobError::Status {
        operation,
        target: target.to_string(),
        status,
        code,
        detail,
    }
}

#[cfg(test)]
#[path = "blob.testing.rs"]
pub(crate) mod testing;

#[cfg(test)]
#[path = "blob.stub.test.rs"]
mod tests;
