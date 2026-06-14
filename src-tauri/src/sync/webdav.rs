use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

use crate::error::{AppError, AppResult};

const BACKUP_FILE: &str = "rssh_backup.enc";

/// WebDAV 配置同步。
#[derive(Debug)]
pub struct WebDavSync {
    pub url: String,
    pub username: String,
    pub password: String,
    client: Client,
}

impl WebDavSync {
    pub fn from_settings(url: &str, username: &str, password: &str) -> AppResult<Self> {
        Self::with_client(url, username, password, Client::new())
    }

    pub fn with_client(
        url: &str,
        username: &str,
        password: &str,
        client: Client,
    ) -> AppResult<Self> {
        if url.is_empty() {
            return Err(AppError::config("webdav_url_missing", json!({})));
        }
        let parsed = Url::parse(url)
            .map_err(|e| AppError::config("webdav_url_invalid", json!({ "err": e.to_string() })))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(AppError::config(
                "webdav_url_invalid",
                json!({ "err": "URL scheme must be http or https" }),
            ));
        }
        if parsed.host_str().map_or(true, |h| h.is_empty()) {
            return Err(AppError::config(
                "webdav_url_invalid",
                json!({ "err": "URL must have a valid host" }),
            ));
        }
        let mut url = parsed.to_string();
        if !url.ends_with('/') {
            url.push('/');
        }
        Ok(Self {
            url,
            username: username.to_string(),
            password: password.to_string(),
            client,
        })
    }

    /// 推送加密配置到 WebDAV。
    pub async fn push(&self, content: &str) -> AppResult<()> {
        let url = format!("{}{}", self.url, BACKUP_FILE);

        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .body(content.to_string())
            .send()
            .await
            .map_err(|e| AppError::other("webdav_push_failed", json!({ "err": e.to_string() })))?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status().as_u16();
        let msg = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED.as_u16() || status == StatusCode::FORBIDDEN.as_u16() {
            return Err(AppError::other(
                "webdav_auth_failed",
                json!({ "status": status }),
            ));
        }

        Err(AppError::other(
            "webdav_api_error",
            json!({ "status": status, "msg": msg }),
        ))
    }

    /// 从 WebDAV 拉取加密配置。
    pub async fn pull(&self) -> AppResult<String> {
        let url = format!("{}{}", self.url, BACKUP_FILE);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| AppError::other("webdav_pull_failed", json!({ "err": e.to_string() })))?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Err(AppError::other("webdav_not_found", json!({})));
        }

        if resp.status().is_success() {
            return resp.text().await.map_err(|e| {
                AppError::other("webdav_pull_failed", json!({ "err": e.to_string() }))
            });
        }

        let status = resp.status().as_u16();
        let msg = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED.as_u16() || status == StatusCode::FORBIDDEN.as_u16() {
            return Err(AppError::other(
                "webdav_auth_failed",
                json!({ "status": status }),
            ));
        }

        Err(AppError::other(
            "webdav_api_error",
            json!({ "status": status, "msg": msg }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_settings_accepts_valid_https_url() {
        let s = WebDavSync::from_settings("https://dav.example.com/rssh/", "u", "p").unwrap();
        assert_eq!(s.url, "https://dav.example.com/rssh/");
        assert_eq!(s.username, "u");
        assert_eq!(s.password, "p");
    }

    #[test]
    fn from_settings_adds_trailing_slash() {
        let s = WebDavSync::from_settings("https://dav.example.com/rssh", "u", "p").unwrap();
        assert_eq!(s.url, "https://dav.example.com/rssh/");
    }

    #[test]
    fn from_settings_rejects_empty_url() {
        let err = WebDavSync::from_settings("", "u", "p").unwrap_err();
        assert_eq!(err.code(), "webdav_url_missing");
    }

    #[test]
    fn from_settings_rejects_invalid_scheme() {
        let err = WebDavSync::from_settings("ftp://dav.example.com/", "u", "p").unwrap_err();
        assert_eq!(err.code(), "webdav_url_invalid");
    }

    #[test]
    fn from_settings_rejects_missing_host() {
        let err = WebDavSync::from_settings("http://", "u", "p").unwrap_err();
        assert_eq!(err.code(), "webdav_url_invalid");
    }

    #[tokio::test]
    async fn push_succeeds_on_201() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("PUT", "/rssh_backup.enc")
            .with_status(201)
            .create_async()
            .await;
        let sync = WebDavSync::from_settings(&server.url(), "u", "p").unwrap();
        sync.push("payload").await.unwrap();
    }

    #[tokio::test]
    async fn pull_returns_body_on_200() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/rssh_backup.enc")
            .with_status(200)
            .with_body("encrypted-data")
            .create_async()
            .await;
        let sync = WebDavSync::from_settings(&server.url(), "u", "p").unwrap();
        assert_eq!(sync.pull().await.unwrap(), "encrypted-data");
    }

    #[tokio::test]
    async fn pull_not_found_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/rssh_backup.enc")
            .with_status(404)
            .create_async()
            .await;
        let sync = WebDavSync::from_settings(&server.url(), "u", "p").unwrap();
        let err = sync.pull().await.unwrap_err();
        assert_eq!(err.code(), "webdav_not_found");
    }

    #[tokio::test]
    async fn push_401_returns_auth_failed() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("PUT", "/rssh_backup.enc")
            .with_status(401)
            .create_async()
            .await;
        let sync = WebDavSync::from_settings(&server.url(), "u", "p").unwrap();
        let err = sync.push("payload").await.unwrap_err();
        assert_eq!(err.code(), "webdav_auth_failed");
    }
}
