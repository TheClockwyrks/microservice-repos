//! A stub Blob service on a loopback socket, for the tests of the Blob client
//! and of the routes over it. It verifies every request's Shared Key signature
//! from the request as it arrived, or a workload-identity bearer token, and
//! keeps the container's blobs in memory.
// cspell:disable

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method as HttpMethod, StatusCode, Uri};
use axum::response::Response;
use base64::Engine as _;
use bytes::Bytes;

use super::{API_VERSION, BlobAuth, BlobClient, BlobConfig, sign};

/// The emulator's account.
pub const ACCOUNT: &str = "devstoreaccount1";
/// Azurite's well-known account key (public, documented by the emulator).
pub const KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
/// The tenant the stub's token endpoint answers for.
pub const TENANT: &str = "tenant-1";

/// The decoded account key.
pub fn key_bytes() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(KEY)
        .unwrap()
}

/// How the stub authenticates a request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// Shared Key, verified against [`KEY`].
    SharedKey,
    /// A bearer token minted by the stub's own token endpoint.
    Bearer,
}

/// The stub service's state.
pub struct Stub {
    auth: Auth,
    /// The containers that exist.
    pub containers: HashSet<String>,
    /// The committed blobs, by container and name.
    pub blobs: BTreeMap<(String, String), Bytes>,
    staged: HashMap<(String, String, String), Bytes>,
    /// `<METHOD> <comp>` of every authenticated request, in order.
    pub log: Vec<String>,
    /// How many tokens the token endpoint minted.
    pub token_calls: usize,
}

/// The stub's state, shared with the test.
pub type Shared = Arc<Mutex<Stub>>;

fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

fn reply(status: u16, code: &str) -> Response {
    let mut response = Response::new(Body::from(format!("<Error><Code>{code}</Code></Error>")));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    if !code.is_empty() {
        response
            .headers_mut()
            .insert("x-ms-error-code", code.parse().unwrap());
    }
    response
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Check the request's credential from the request exactly as it arrived: the
/// refusal, or `None` to proceed.
fn authenticate(
    auth: Auth,
    method: &HttpMethod,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Response> {
    let authorization = header(headers, "authorization").unwrap_or_default();
    match auth {
        Auth::SharedKey => {
            let received: Vec<(String, String)> = headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        value.to_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            let query = query_pairs(uri);
            let string_to_sign = sign::string_to_sign(&sign::SignedRequest {
                method: method.as_str(),
                headers: &received,
                account: ACCOUNT,
                path: uri.path(),
                query: &query,
            });
            let expected = format!(
                "SharedKey {ACCOUNT}:{}",
                sign::signature(&key_bytes(), &string_to_sign)
            );
            if authorization != expected {
                return Some(reply(403, "AuthenticationFailed"));
            }
        }
        Auth::Bearer => {
            if !authorization.starts_with("Bearer token-") {
                return Some(reply(401, "InvalidAuthenticationInfo"));
            }
        }
    }
    if header(headers, "x-ms-version").as_deref() != Some(API_VERSION) {
        return Some(reply(400, "InvalidHeaderValue"));
    }
    None
}

async fn handle(
    State(stub): State<Shared>,
    method: HttpMethod,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut stub = stub.lock().unwrap();
    let path = uri.path().to_string();

    if path == format!("/{TENANT}/oauth2/v2.0/token") {
        let form = String::from_utf8_lossy(&body).to_string();
        assert!(
            form.contains("client_assertion=projected-sa-token"),
            "{form}"
        );
        stub.token_calls += 1;
        return Response::new(Body::from(format!(
            r#"{{"access_token":"token-{}","expires_in":3600}}"#,
            stub.token_calls
        )));
    }

    if let Some(response) = authenticate(stub.auth, &method, &uri, &headers) {
        return response;
    }
    let Some(rest) = path.strip_prefix(&format!("/{ACCOUNT}/")) else {
        return reply(400, "InvalidUri");
    };
    let (container, name) = match rest.split_once('/') {
        Some((container, name)) => (decode(container), Some(decode(name))),
        None => (decode(rest), None),
    };
    let query: HashMap<String, String> = query_pairs(&uri).into_iter().collect();
    let comp = query.get("comp").cloned().unwrap_or_default();
    stub.log
        .push(format!("{method} {comp}").trim_end().to_string());

    if name.is_none() && query.get("restype").map(String::as_str) == Some("container") {
        if method == HttpMethod::PUT {
            return if stub.containers.insert(container) {
                reply(201, "")
            } else {
                reply(409, "ContainerAlreadyExists")
            };
        }
        if method == HttpMethod::GET && comp == "list" {
            if !stub.containers.contains(&container) {
                return reply(404, "ContainerNotFound");
            }
            return Response::new(Body::from(
                "<?xml version=\"1.0\"?><EnumerationResults><Blobs></Blobs><NextMarker /></EnumerationResults>",
            ));
        }
        return reply(400, "Unsupported");
    }
    let Some(name) = name else {
        return reply(400, "Unsupported");
    };
    if !stub.containers.contains(&container) {
        return reply(404, "ContainerNotFound");
    }
    let id = (container.clone(), name.clone());
    match (method, comp.as_str()) {
        (HttpMethod::PUT, "block") => {
            let block = query.get("blockid").cloned().unwrap_or_default();
            if header(&headers, "content-length") != Some(body.len().to_string()) {
                return reply(400, "MissingContentLengthHeader");
            }
            stub.staged.insert((container, name, block), body);
            reply(201, "")
        }
        (HttpMethod::PUT, "blocklist") => {
            let list = String::from_utf8_lossy(&body).to_string();
            let mut data = Vec::new();
            for part in list.split("<Latest>").skip(1) {
                let block = part.split("</Latest>").next().unwrap_or("").to_string();
                match stub.staged.get(&(container.clone(), name.clone(), block)) {
                    Some(bytes) => data.extend_from_slice(bytes),
                    None => return reply(400, "InvalidBlockList"),
                }
            }
            stub.staged
                .retain(|(c, n, _), _| !(c == &container && n == &name));
            stub.blobs.insert(id, Bytes::from(data));
            reply(201, "")
        }
        (HttpMethod::PUT, "") => {
            if header(&headers, "x-ms-blob-type").as_deref() != Some("BlockBlob") {
                return reply(400, "MissingRequiredHeader");
            }
            stub.blobs.insert(id, body);
            reply(201, "")
        }
        (HttpMethod::GET, "") => match stub.blobs.get(&id) {
            None => reply(404, "BlobNotFound"),
            Some(data) => {
                let mut response = Response::new(Body::from(data.clone()));
                response
                    .headers_mut()
                    .insert("content-length", data.len().to_string().parse().unwrap());
                response
            }
        },
        (HttpMethod::DELETE, "") => {
            if header(&headers, "x-ms-delete-snapshots").as_deref() != Some("include") {
                return reply(400, "MissingRequiredHeader");
            }
            match stub.blobs.remove(&id) {
                Some(_) => reply(202, ""),
                None => reply(404, "BlobNotFound"),
            }
        }
        _ => reply(400, "Unsupported"),
    }
}

/// Serve a stub on a loopback port, answering its origin and state.
pub async fn serve(auth: Auth) -> (String, Shared) {
    let stub = Arc::new(Mutex::new(Stub {
        auth,
        containers: HashSet::new(),
        blobs: BTreeMap::new(),
        staged: HashMap::new(),
        log: Vec::new(),
        token_calls: 0,
    }));
    let app = axum::Router::new()
        .fallback(handle)
        .with_state(Arc::clone(&stub))
        .layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), stub)
}

/// The configuration of `container` on the stub at `origin`, signed by Shared
/// Key as Azurite is.
pub fn shared_key_config(origin: &str, container: &str) -> BlobConfig {
    BlobConfig {
        account: ACCOUNT.to_string(),
        container: container.to_string(),
        endpoint: format!("{origin}/{ACCOUNT}/"),
        auth: BlobAuth::SharedKey(key_bytes()),
    }
}

/// A Shared Key client of `container` on the stub at `origin`.
pub fn shared_key_client(origin: &str, container: &str) -> BlobClient {
    BlobClient::new(
        &shared_key_config(origin, container),
        reqwest::Client::new(),
    )
    .unwrap()
}
