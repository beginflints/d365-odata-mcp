//! Authentication module
//!
//! Implements OAuth2 Client Credentials flow for:
//! - Azure AD (Entra ID) - for cloud D365
//! - ADFS - for on-premise D365

use reqwest::{Client, Url};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::RwLock;

/// Authentication errors
#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Token request failed: {0}")]
    TokenRequestFailed(String),

    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Token parse error: {0}")]
    ParseError(String),

    #[error("Missing credentials: {0}")]
    MissingCredentials(String),
}

/// Token response from OAuth2 server
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: u64,
    #[allow(dead_code)]
    #[serde(default)]
    ext_expires_in: u64,
}

/// Cached token with expiry tracking
#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        // Consider token expired 60 seconds before actual expiry
        self.expires_at > Instant::now() + Duration::from_secs(60)
    }
}

/// Authentication type
#[derive(Debug, Clone, PartialEq)]
pub enum AuthType {
    /// Azure AD (Entra ID) - for cloud D365
    AzureAd,
    /// ADFS - for on-premise D365
    Adfs,
}

impl Default for AuthType {
    fn default() -> Self {
        AuthType::AzureAd
    }
}

impl std::str::FromStr for AuthType {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "azure" | "azuread" | "azure_ad" | "entra" => Ok(AuthType::AzureAd),
            "adfs" | "on-premise" | "onpremise" => Ok(AuthType::Adfs),
            _ => Err(format!("Unknown auth type: {}. Use 'azure' or 'adfs'", s)),
        }
    }
}

/// Authentication configuration
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub auth_type: AuthType,
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    /// Custom token URL (required for ADFS)
    pub token_url: Option<String>,
    /// Resource/audience (required for ADFS)
    pub resource: Option<String>,
}

/// Unified OAuth2 authentication helper
#[derive(Debug)]
pub struct OAuth2Auth {
    config: AuthConfig,
    http_client: Client,
    token_cache: Arc<RwLock<Option<CachedToken>>>,
}

impl OAuth2Auth {
    /// Create a new OAuth2 auth helper
    pub fn new(config: AuthConfig) -> Self {
        Self {
            config,
            http_client: Client::new(),
            token_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Get the token endpoint URL
    fn token_endpoint(&self) -> String {
        match self.config.auth_type {
            AuthType::Adfs => {
                // ADFS requires custom token URL
                self.config.token_url.clone().unwrap_or_else(|| {
                    format!("https://{}/adfs/oauth2/token", self.config.tenant_id)
                })
            }
            AuthType::AzureAd => {
                // Azure AD standard endpoint
                format!(
                    "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                    self.config.tenant_id
                )
            }
        }
    }

    /// Acquire or return a cached access token for the given resource.
    pub async fn get_token(&self, resource: &str) -> Result<String, AuthError> {
        // Check cache first
        {
            let cache = self.token_cache.read().await;
            if let Some(ref cached) = *cache {
                if cached.is_valid() {
                    tracing::debug!("Using cached token");
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Token expired or not cached, acquire new one
        tracing::info!("Acquiring new access token");
        let token = self.acquire_token(resource).await?;

        Ok(token)
    }

    /// Acquire a new token
    async fn acquire_token(&self, resource: &str) -> Result<String, AuthError> {
        let params = match self.config.auth_type {
            AuthType::AzureAd => {
                // Azure AD uses scope with /.default suffix
                let scope = if resource.ends_with('/') {
                    format!("{}.default", resource)
                } else {
                    format!("{}/.default", resource)
                };
                
                vec![
                    ("grant_type".to_string(), "client_credentials".to_string()),
                    ("client_id".to_string(), self.config.client_id.clone()),
                    ("client_secret".to_string(), self.config.client_secret.clone()),
                    ("scope".to_string(), scope),
                ]
            }
            AuthType::Adfs => {
                // ADFS uses resource parameter instead of scope
                let resource = self.config.resource.as_ref()
                    .map(|r| r.clone())
                    .unwrap_or_else(|| resource.to_string());
                
                vec![
                    ("grant_type".to_string(), "client_credentials".to_string()),
                    ("client_id".to_string(), self.config.client_id.clone()),
                    ("client_secret".to_string(), self.config.client_secret.clone()),
                    ("resource".to_string(), resource),
                ]
            }
        };

        tracing::debug!("Token endpoint: {}", self.token_endpoint());
        tracing::debug!("Auth type: {:?}", self.config.auth_type);

        let response = self
            .http_client
            .post(&self.token_endpoint())
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!("Token request failed: {} - {}", status, body);
            return Err(AuthError::TokenRequestFailed(format!(
                "Status: {}, Body: {}",
                status, body
            )));
        }

        let token_response: TokenResponse = response.json().await.map_err(|e| {
            AuthError::ParseError(format!("Failed to parse token response: {}", e))
        })?;

        // Cache the token
        let cached = CachedToken {
            access_token: token_response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(token_response.expires_in),
        };

        {
            let mut cache = self.token_cache.write().await;
            *cache = Some(cached);
        }

        tracing::info!(
            "Token acquired successfully, expires in {} seconds",
            token_response.expires_in
        );

        Ok(token_response.access_token)
    }

    /// Clear the token cache
    pub async fn clear_cache(&self) {
        let mut cache = self.token_cache.write().await;
        *cache = None;
    }

    /// Get resource URL from endpoint
    pub fn resource_from_endpoint(endpoint: &str) -> String {
        if let Ok(url) = Url::parse(endpoint) {
            format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""))
        } else {
            endpoint
                .split('/')
                .take(3)
                .collect::<Vec<_>>()
                .join("/")
        }
    }
}

// Keep AzureAdAuth for backward compatibility
pub type AzureAdAuth = OAuth2Auth;

impl OAuth2Auth {
    /// Create a new Azure AD auth helper (backward compatible)
    pub fn new_azure(tenant_id: String, client_id: String, client_secret: String) -> Self {
        OAuth2Auth::new(AuthConfig {
            auth_type: AuthType::AzureAd,
            tenant_id,
            client_id,
            client_secret,
            token_url: None,
            resource: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_azure_auth() {
        let auth = AzureAdAuth::new(
            "tenant-id".to_string(),
            "client-id".to_string(),
            "secret".to_string(),
        );
        assert_eq!(auth.config.tenant_id, "tenant-id");
        assert_eq!(auth.config.client_id, "client-id");
        assert_eq!(auth.config.auth_type, AuthType::AzureAd);
    }

    #[test]
    fn test_create_adfs_auth() {
        let auth = OAuth2Auth::new(AuthConfig {
            auth_type: AuthType::Adfs,
            tenant_id: "adfs".to_string(),
            client_id: "client-id".to_string(),
            client_secret: "secret".to_string(),
            token_url: Some("https://fs.example.com/adfs/oauth2/token".to_string()),
            resource: Some("https://d365.example.com".to_string()),
        });
        assert_eq!(auth.config.auth_type, AuthType::Adfs);
        assert_eq!(auth.token_endpoint(), "https://fs.example.com/adfs/oauth2/token");
    }

    #[test]
    fn test_azure_token_endpoint() {
        let auth = AzureAdAuth::new(
            "my-tenant".to_string(),
            "client-id".to_string(),
            "secret".to_string(),
        );
        assert_eq!(
            auth.token_endpoint(),
            "https://login.microsoftonline.com/my-tenant/oauth2/v2.0/token"
        );
    }

    #[test]
    fn test_auth_type_from_str() {
        assert_eq!("azure".parse::<AuthType>().unwrap(), AuthType::AzureAd);
        assert_eq!("adfs".parse::<AuthType>().unwrap(), AuthType::Adfs);
        assert_eq!("ADFS".parse::<AuthType>().unwrap(), AuthType::Adfs);
    }

    #[test]
    fn test_resource_from_endpoint() {
        assert_eq!(
            OAuth2Auth::resource_from_endpoint("https://org.crm.dynamics.com/api/data/v9.2/"),
            "https://org.crm.dynamics.com"
        );
        assert_eq!(
            OAuth2Auth::resource_from_endpoint("https://org.operations.dynamics.com/data/"),
            "https://org.operations.dynamics.com"
        );
    }

    #[test]
    fn test_cached_token_validity() {
        let valid_token = CachedToken {
            access_token: "test".to_string(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(valid_token.is_valid());

        let expired_token = CachedToken {
            access_token: "test".to_string(),
            expires_at: Instant::now() - Duration::from_secs(60),
        };
        assert!(!expired_token.is_valid());
    }
}
