//! Configuration module for D365 OData MCP
//!
//! Loads configuration from TOML file and environment variables.
//! Environment variables take precedence over file config.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;

/// Product type - Dataverse or Finance & Operations
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProductType {
    Dataverse,
    #[serde(alias = "fno", alias = "fo")]
    Finops,
}

impl Default for ProductType {
    fn default() -> Self {
        ProductType::Dataverse
    }
}

/// Global configuration settings
#[derive(Debug, Deserialize, Clone)]
pub struct GlobalConfig {
    #[serde(default)]
    pub product: ProductType,
    pub endpoint: String,
    #[serde(default)]
    pub page_size: Option<usize>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub retry_delay_ms: Option<u64>,
}

/// Observability configuration
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub enable_tracing: Option<bool>,
}

/// Delta sync storage configuration
#[derive(Debug, Deserialize, Clone, Default)]
pub struct DeltaConfig {
    #[serde(default)]
    pub storage_path: Option<String>,
}

/// Entity-specific configuration
#[derive(Debug, Deserialize, Clone)]
pub struct EntityConfig {
    pub name: String,
    #[serde(default)]
    pub initial_load: Option<bool>,
    #[serde(default)]
    pub delta_enabled: Option<bool>,
    #[serde(default)]
    pub cross_company: Option<bool>,
}

/// Root configuration structure
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub global: GlobalConfig,
    #[serde(default)]
    pub observability: Option<ObservabilityConfig>,
    #[serde(default)]
    pub delta: Option<DeltaConfig>,
    #[serde(default)]
    pub entities: Option<Vec<EntityConfig>>,
}

/// Runtime configuration with resolved values from env vars
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub product: ProductType,
    pub endpoint: String,
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    /// Authentication type: "azure" or "adfs"
    pub auth_type: String,
    /// Custom token URL (for ADFS)
    pub token_url: Option<String>,
    /// Resource/audience (for ADFS)
    pub resource: Option<String>,
    pub page_size: usize,
    pub concurrency: usize,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub log_level: String,
    pub enable_tracing: bool,
    pub delta_storage_path: String,
    pub entities: Vec<EntityConfig>,
}

impl Config {
    /// Load configuration from a TOML file path
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load from default path or create default config
    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        let default_path = "config/default.toml";
        if Path::new(default_path).exists() {
            Self::load_from_path(default_path)
        } else {
            // Minimal default config
            Ok(Config {
                global: GlobalConfig {
                    product: ProductType::default(),
                    endpoint: String::new(),
                    page_size: Some(500),
                    concurrency: Some(4),
                    max_retries: Some(3),
                    retry_delay_ms: Some(1000),
                },
                observability: Some(ObservabilityConfig::default()),
                delta: Some(DeltaConfig::default()),
                entities: None,
            })
        }
    }

    /// Resolve configuration with environment variables
    /// Environment variables take precedence over file config
    pub fn to_runtime(&self) -> Result<RuntimeConfig, Box<dyn std::error::Error>> {
        // Required env vars (no defaults)
        let tenant_id = env::var("TENANT_ID")
            .map_err(|_| "TENANT_ID environment variable is required")?;
        let client_id = env::var("CLIENT_ID")
            .map_err(|_| "CLIENT_ID environment variable is required")?;
        let client_secret = env::var("CLIENT_SECRET")
            .map_err(|_| "CLIENT_SECRET environment variable is required")?;

        // Optional env vars with fallback to config file
        let endpoint = env::var("ENDPOINT").unwrap_or_else(|_| self.global.endpoint.clone());
        if endpoint.is_empty() {
            return Err("ENDPOINT environment variable or config endpoint is required".into());
        }

        let product = env::var("PRODUCT")
            .ok()
            .and_then(|p| match p.to_lowercase().as_str() {
                "dataverse" => Some(ProductType::Dataverse),
                "finops" | "fno" | "fo" => Some(ProductType::Finops),
                _ => None,
            })
            .unwrap_or_else(|| self.global.product.clone());

        let obs = self.observability.clone().unwrap_or_default();
        let delta = self.delta.clone().unwrap_or_default();

        // Auth type (azure or adfs)
        let auth_type = env::var("AUTH_TYPE").unwrap_or_else(|_| "azure".to_string());
        
        // Custom token URL (for ADFS)
        let token_url = env::var("TOKEN_URL").ok();
        
        // Resource/audience (for ADFS) 
        let resource = env::var("RESOURCE").ok();

        Ok(RuntimeConfig {
            product,
            endpoint,
            tenant_id,
            client_id,
            client_secret,
            auth_type,
            token_url,
            resource,
            page_size: self.global.page_size.unwrap_or(500),
            concurrency: self.global.concurrency.unwrap_or(4),
            max_retries: self.global.max_retries.unwrap_or(3),
            retry_delay_ms: self.global.retry_delay_ms.unwrap_or(1000),
            log_level: obs.log_level.unwrap_or_else(|| "info".to_string()),
            enable_tracing: obs.enable_tracing.unwrap_or(false),
            delta_storage_path: delta.storage_path.unwrap_or_else(|| "./delta_state.json".to_string()),
            entities: self.entities.clone().unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_product_type_default() {
        assert_eq!(ProductType::default(), ProductType::Dataverse);
    }

    #[test]
    fn test_product_type_deserialize() {
        #[derive(Deserialize)]
        struct Test {
            product: ProductType,
        }

        let toml_str = r#"product = "finops""#;
        let test: Test = toml::from_str(toml_str).unwrap();
        assert_eq!(test.product, ProductType::Finops);

        let toml_str = r#"product = "dataverse""#;
        let test: Test = toml::from_str(toml_str).unwrap();
        assert_eq!(test.product, ProductType::Dataverse);
    }
}
