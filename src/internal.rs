//! Direct transport for Spotify's native services, independent of Web API grants.
//! Credentials and responses containing tokens never enter diagnostics.
//! Persisted operation identifiers were verified against the public Spotify
//! Web Player bundles on 2026-10-12. Only identifiers are retained, not its code.

mod adapter;
mod cache;

use std::sync::Arc;
use std::time::{Duration, Instant};

use librespot_core::authentication::Credentials;
use librespot_protocol::client_info::ClientInfo;
use librespot_protocol::clienttoken_http::{
    ClientTokenRequest, ClientTokenRequestType, ClientTokenResponse,
};
use librespot_protocol::credentials::StoredCredential;
use librespot_protocol::login5::{LoginRequest, LoginResponse, login_request::Login_method};
use protobuf::{Message, MessageField};
use reqwest::{Method, StatusCode};
use tokio::sync::{Mutex, Semaphore};

use crate::api::{ApiError, client::Result};
use crate::http::Http;

const CLIENT_TOKEN_URL: &str = "https://clienttoken.spotify.com/v1/clienttoken";
const LOGIN_URL: &str = "https://login5.spotify.com/v3/login";
const CLIENT_VERSION: &str = "1.2.52.442";

struct Authorization {
    credentials: Credentials,
    access_token: String,
    client_token: String,
    expires: Instant,
    base_url: String,
}

/// This transport cannot fall back to a public Web API application.
pub struct Client {
    http: Http,
    device_id: String,
    authorization: Mutex<Authorization>,
    cooldown: Mutex<Instant>,
    requests: Semaphore,
    origin: std::sync::RwLock<String>,
    cache: Mutex<cache::MetadataCache>,
}

fn protocol_error() -> ApiError {
    ApiError::Decode("Spotify's native service returned an unsupported response".into())
}

fn network(error: reqwest::Error) -> ApiError {
    ApiError::Network(error.without_url().to_string())
}

fn status_error(status: StatusCode) -> ApiError {
    ApiError::Status {
        status: status.as_u16(),
        message: format!("Spotify native service: HTTP {}", status.as_u16()),
    }
}

impl Client {
    pub fn new(http: Http, credentials: Credentials) -> Arc<Self> {
        let bytes: [u8; 20] = rand::random();
        let device_id = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Arc::new(Self {
            http,
            device_id,
            authorization: Mutex::new(Authorization {
                credentials,
                access_token: String::new(),
                client_token: String::new(),
                expires: Instant::now(),
                base_url: String::new(),
            }),
            cooldown: Mutex::new(Instant::now()),
            requests: Semaphore::new(6),
            origin: std::sync::RwLock::new(String::new()),
            cache: Mutex::new(cache::MetadataCache::default()),
        })
    }

    async fn response(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        let _permit = self
            .requests
            .acquire()
            .await
            .map_err(|_| ApiError::NotSignedIn)?;
        loop {
            let deadline = *self.cooldown.lock().await;
            if deadline <= Instant::now() {
                break;
            }
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        let http = self.http.client().map_err(ApiError::Network)?;
        let mut request = http.request(method, url).timeout(Duration::from_secs(30));
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(network)?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(protocol_error)?;
            let mut cooldown = self.cooldown.lock().await;
            *cooldown = (*cooldown).max(deadline);
            return Err(ApiError::RateLimited);
        }
        if status == StatusCode::BAD_REQUEST {
            let bytes = response.bytes().await.map_err(network)?;
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if url.contains("/connect-state/v1/") {
                    let message = value
                        .pointer("/error/message")
                        .or_else(|| value.get("message"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Invalid Connect command");
                    let safe = message
                        .split_whitespace()
                        .take(30)
                        .map(|word| {
                            if word.len() > 24 || word.contains(':') {
                                "[redacted]"
                            } else {
                                word
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    return Err(ApiError::Status {
                        status: 400,
                        message: format!("Spotify Connect: {safe}"),
                    });
                }
                let variables = value
                    .get("errors")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|error| error.get("message").and_then(serde_json::Value::as_str))
                    .filter_map(|message| {
                        message.split_once('$').map(|(_, tail)| {
                            tail.chars()
                                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                                .collect::<String>()
                        })
                    })
                    .collect::<Vec<_>>();
                if !variables.is_empty() {
                    return Err(ApiError::Decode(format!(
                        "Native operation requires variables: {}",
                        variables.join(", ")
                    )));
                }
            }
            return Err(status_error(status));
        }
        if status == StatusCode::UNAUTHORIZED
            && headers.iter().any(|(name, _)| *name == "Authorization")
        {
            // A refresh holds the authorization lock while requesting a token.
            // Release our network slot before waiting for that lock.
            drop(_permit);
            self.authorization.lock().await.expires = Instant::now();
            return Err(ApiError::SignInExpired {
                api_source: crate::api::ApiSource::Shared,
            });
        }
        if !status.is_success() {
            return Err(status_error(status));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(network)
    }

    async fn authenticate(&self, state: &mut Authorization) -> Result<()> {
        let mut request = ClientTokenRequest::new();
        request.request_type = ClientTokenRequestType::REQUEST_CLIENT_DATA_REQUEST.into();
        let data = request.mut_client_data();
        data.client_id = crate::auth::PLAYBACK_CLIENT_ID.into();
        data.client_version = CLIENT_VERSION.into();
        let sdk = data.mut_connectivity_sdk_data();
        sdk.device_id = self.device_id.clone();
        let platform = sdk.platform_specific_data.mut_or_insert_default();
        #[cfg(windows)]
        {
            let windows = platform.mut_desktop_windows();
            windows.os_version = 10;
            windows.os_build = 21370;
            windows.platform_id = 2;
            windows.unknown_value_6 = 9;
            windows.image_file_machine = if cfg!(target_arch = "aarch64") {
                43620
            } else {
                34404
            };
            windows.pe_machine = windows.image_file_machine;
            windows.unknown_value_10 = true;
        }
        #[cfg(target_os = "macos")]
        {
            let mac = platform.mut_desktop_macos();
            mac.system_version = "14.0".into();
            mac.hw_model = "Mac".into();
            mac.compiled_cpu_type = std::env::consts::ARCH.into();
        }
        #[cfg(target_os = "linux")]
        {
            let linux = platform.mut_desktop_linux();
            linux.system_name = "Linux".into();
            linux.hardware = std::env::consts::ARCH.into();
        }
        let body = request.write_to_bytes().map_err(|_| protocol_error())?;
        let bytes = self
            .response(
                Method::POST,
                CLIENT_TOKEN_URL,
                Some(body),
                &[
                    ("Accept", "application/x-protobuf"),
                    ("Content-Type", "application/x-protobuf"),
                ],
            )
            .await?;
        let response =
            ClientTokenResponse::parse_from_bytes(&bytes).map_err(|_| protocol_error())?;
        if !response.has_granted_token() {
            return Err(protocol_error());
        }
        let client_token = response.granted_token().token.clone();
        let username = state
            .credentials
            .username
            .clone()
            .ok_or(ApiError::NotSignedIn)?;
        let request = LoginRequest {
            client_info: MessageField::some(ClientInfo {
                client_id: crate::auth::PLAYBACK_CLIENT_ID.into(),
                device_id: self.device_id.clone(),
                ..Default::default()
            }),
            login_method: Some(Login_method::StoredCredential(StoredCredential {
                username,
                data: state.credentials.auth_data.clone(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let body = request.write_to_bytes().map_err(|_| protocol_error())?;
        let bytes = self
            .response(
                Method::POST,
                LOGIN_URL,
                Some(body),
                &[
                    ("Accept", "application/x-protobuf"),
                    ("Content-Type", "application/x-protobuf"),
                    ("client-token", &client_token),
                ],
            )
            .await?;
        let response = LoginResponse::parse_from_bytes(&bytes).map_err(|_| protocol_error())?;
        if !response.has_ok() {
            return Err(protocol_error());
        }
        let login = response.ok();
        if login.access_token.is_empty()
            || login.username != state.credentials.username.as_deref().unwrap_or_default()
        {
            return Err(ApiError::NotSignedIn);
        }
        #[derive(serde::Deserialize)]
        struct Resolver {
            spclient: Vec<String>,
        }
        let bytes = self
            .response(
                Method::GET,
                "https://apresolve.spotify.com/?type=spclient",
                None,
                &[],
            )
            .await?;
        let resolver: Resolver = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        let authority = resolver.spclient.first().ok_or_else(protocol_error)?;
        let base_url = format!("https://{authority}");
        validate_service_url(&base_url)?;
        state.base_url = base_url;
        state.client_token = client_token;
        state.access_token = login.access_token.clone();
        state.expires = Instant::now()
            + Duration::from_secs(
                u64::try_from(login.access_token_expires_in)
                    .map_err(|_| protocol_error())?
                    .saturating_sub(60)
                    .max(1),
            );
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<Vec<u8>> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(protocol_error());
        }
        let (base_url, access_token, client_token) = {
            let mut state = self.authorization.lock().await;
            if state.expires <= Instant::now() {
                self.authenticate(&mut state).await?;
            }
            (
                state.base_url.clone(),
                state.access_token.clone(),
                state.client_token.clone(),
            )
        };
        self.response(
            Method::GET,
            &format!("{base_url}{path}"),
            None,
            &[
                ("Authorization", &format!("Bearer {access_token}")),
                ("client-token", &client_token),
                ("Accept", "application/x-protobuf"),
            ],
        )
        .await
    }

    pub async fn username(&self) -> Result<String> {
        self.authorization
            .lock()
            .await
            .credentials
            .username
            .clone()
            .ok_or(ApiError::NotSignedIn)
    }

    pub fn set_device_id(&self, device_id: &str) {
        *self.origin.write().unwrap_or_else(|p| p.into_inner()) = device_id.to_owned();
    }

    pub async fn upload_cover(&self, id: &str, encoded: &str) -> Result<()> {
        use base64::Engine as _;
        let body = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| protocol_error())?;
        let (access, client) = {
            let mut state = self.authorization.lock().await;
            if state.expires <= Instant::now() {
                self.authenticate(&mut state).await?;
            }
            (state.access_token.clone(), state.client_token.clone())
        };
        let bytes = self
            .response(
                Method::POST,
                "https://image-upload.spotify.com/v4/playlist",
                Some(body),
                &[
                    ("Authorization", &format!("Bearer {access}")),
                    ("client-token", &client),
                    ("Content-Type", "image/jpeg"),
                ],
            )
            .await?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        let token = value["uploadToken"].as_str().ok_or_else(protocol_error)?;
        let image = self
            .json(
                Method::POST,
                &format!("/playlist/v2/playlist/{id}/register-image"),
                Some(serde_json::json!({"uploadToken":token})),
            )
            .await?;
        let picture = image["picture"].as_str().ok_or_else(protocol_error)?;
        self.json(Method::POST,&format!("/playlist/v2/playlist/{id}/changes"),Some(serde_json::json!({"deltas":[{"ops":[{"kind":"UPDATE_LIST_ATTRIBUTES","updateListAttributes":{"newAttributes":{"values":{"picture":picture,"formatAttributes":[],"pictureSize":[]},"noValue":[]}}}],"info":{"source":{"client":"WEBPLAYER"}}}],"wantResultingRevisions":false,"wantSyncResult":false,"nonces":[]}))).await?;
        Ok(())
    }

    pub async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(protocol_error());
        }
        let (access, client) = {
            let mut state = self.authorization.lock().await;
            if state.expires <= Instant::now() {
                self.authenticate(&mut state).await?;
            }
            (state.access_token.clone(), state.client_token.clone())
        };
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|_| protocol_error())?;
        let bytes = self
            .response(
                method,
                &format!("https://spclient.wg.spotify.com{path}"),
                body,
                &[
                    ("Authorization", &format!("Bearer {access}")),
                    ("client-token", &client),
                    ("Accept", "application/json"),
                    ("Content-Type", "application/json"),
                ],
            )
            .await?;
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    pub async fn operation(
        &self,
        name: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        static HASHES: std::sync::OnceLock<std::collections::HashMap<String, String>> =
            std::sync::OnceLock::new();
        let hashes = HASHES.get_or_init(|| {
            serde_json::from_str(include_str!("internal/operations.json"))
                .expect("embedded operation manifest")
        });
        let hash = hashes.get(name).ok_or_else(protocol_error)?;
        let cached = matches!(
            name,
            "getTrack"
                | "getEpisodeOrChapter"
                | "getAlbum"
                | "queryShowMetadataV2"
                | "queryArtistOverview"
        );
        let key = format!("{name}:{}", variables);
        if cached
            && let Some((time, value)) = self.cache.lock().await.get(&key)
            && time.elapsed() < Duration::from_secs(300)
        {
            return Ok(value.clone());
        }
        let value = self.graphql(name, hash, variables).await?;
        if cached {
            let mut cache = self.cache.lock().await;
            cache.insert(key, (Instant::now(), value.clone()));
        }
        Ok(value)
    }

    pub async fn rootlist(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<librespot_protocol::playlist4_external::SelectedListContent> {
        let username = self
            .authorization
            .lock()
            .await
            .credentials
            .username
            .clone()
            .ok_or(ApiError::NotSignedIn)?;
        let bytes = self.get(&format!("/playlist/v2/user/{}/rootlist?decorate=revision,attributes,length,owner,capabilities,status_code&from={offset}&length={limit}", urlencoding::encode(&username))).await?;
        librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&bytes)
            .map_err(|_| protocol_error())
    }

    pub async fn graphql(
        &self,
        operation: &str,
        hash: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (access_token, client_token) = {
            let mut state = self.authorization.lock().await;
            if state.expires <= Instant::now() {
                self.authenticate(&mut state).await?;
            }
            (state.access_token.clone(), state.client_token.clone())
        };
        let body = serde_json::to_vec(&serde_json::json!({"operationName":operation,"variables":variables,"extensions":{"persistedQuery":{"version":1,"sha256Hash":hash}}})).map_err(|_| protocol_error())?;
        let bytes = self
            .response(
                Method::POST,
                "https://api-partner.spotify.com/pathfinder/v2/query",
                Some(body),
                &[
                    ("Authorization", &format!("Bearer {access_token}")),
                    ("client-token", &client_token),
                    ("Accept", "application/json"),
                    ("Content-Type", "application/json"),
                ],
            )
            .await?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        if value
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            return Err(protocol_error());
        }
        value.get("data").cloned().ok_or_else(protocol_error)
    }
}

fn validate_service_url(url: &str) -> Result<()> {
    let url = reqwest::Url::parse(url).map_err(|_| protocol_error())?;
    if url.scheme() != "https"
        || !url
            .host_str()
            .is_some_and(|host| host.ends_with(".spotify.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(protocol_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_requests_cannot_deadlock_token_refresh() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let ready = Arc::new(tokio::sync::Notify::new());
        let server_ready = ready.clone();
        let server = tokio::spawn(async move {
            for index in 0..7 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                let response = if index == 6 {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                if index == 5 {
                    server_ready.notify_one();
                }
            }
        });
        let client = Client::new(Http::default(), Credentials::with_access_token("test"));
        let authorization = client.authorization.lock().await;
        let mut requests = Vec::new();
        for _ in 0..6 {
            let client = client.clone();
            let url = url.clone();
            requests.push(tokio::spawn(async move {
                client
                    .response(Method::GET, &url, None, &[("Authorization", "Bearer test")])
                    .await
            }));
        }
        ready.notified().await;
        let refresh = tokio::time::timeout(
            Duration::from_secs(2),
            client.response(Method::POST, &url, None, &[]),
        )
        .await;
        assert_eq!(refresh.unwrap().unwrap(), b"ok");
        drop(authorization);
        for request in requests {
            assert!(matches!(
                request.await.unwrap(),
                Err(ApiError::SignInExpired { .. })
            ));
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_pauses_other_operations_without_repeating_a_mutation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = Client::new(Http::default(), Credentials::with_access_token("test"));
        assert!(matches!(
            client.response(Method::POST, &url, None, &[]).await,
            Err(ApiError::RateLimited)
        ));
        let start = Instant::now();
        assert_eq!(
            client.response(Method::GET, &url, None, &[]).await.unwrap(),
            b"ok"
        );
        assert!(start.elapsed() >= Duration::from_millis(900));
        server.await.unwrap();
    }

    #[test]
    fn resolver_cannot_send_credentials_to_another_host() {
        assert!(validate_service_url("https://gew4-spclient.spotify.com:443").is_ok());
        for url in [
            "http://gew4-spclient.spotify.com",
            "https://spotify.com.attacker.test",
            "https://attacker.test@gew4-spclient.spotify.com",
            "https://gew4-spclient.spotify.com/path",
            "https://gew4-spclient.spotify.com?x=1",
        ] {
            assert!(validate_service_url(url).is_err());
        }
    }
}
