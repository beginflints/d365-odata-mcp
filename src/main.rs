//! D365 OData MCP Server
//!
//! Entry point for the MCP server binary.
//! Implements MCP protocol over stdio using JSON-RPC 2.0.

use d365_odata_mcp::config::Config;
use d365_odata_mcp::mcp::{
    CallToolParams, CallToolResult, D365McpServer, InitializeResult, JsonRpcRequest,
    JsonRpcResponse, ListToolsResult, ServerCapabilities, ServerInfo, ToolsCapability,
};
use d365_odata_mcp::odata::ODataClient;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn log_to_file(msg: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/d365-mcp.log")
    {
        let _ = writeln!(file, "[{}] {}", chrono_lite(), msg);
    }
}

fn chrono_lite() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

fn main() {
    log_to_file("=== MCP Server Starting ===");
    log_to_file(&format!("Args: {:?}", env::args().collect::<Vec<_>>()));
    
    // Handle --version and --help flags before starting async runtime
    let args: Vec<String> = env::args().collect();
    
    if args.len() > 1 {
        match args[1].as_str() {
            "--version" | "-V" | "-v" => {
                println!("d365-odata-mcp {}", env!("CARGO_PKG_VERSION"));
                log_to_file("Exiting: --version flag");
                return;
            }
            "--help" | "-h" => {
                println!("d365-odata-mcp {}", env!("CARGO_PKG_VERSION"));
                println!("MCP Server for Microsoft Dynamics 365 OData API\n");
                println!("Usage: d365-odata-mcp\n");
                println!("Environment variables:");
                println!("  TENANT_ID      Azure AD tenant ID (required)");
                println!("  CLIENT_ID      Azure AD client/app ID (required)");
                println!("  CLIENT_SECRET  Azure AD client secret (required)");
                println!("  ENDPOINT       D365 OData endpoint URL (required)");
                println!("  PRODUCT        'dataverse' or 'finops' (required)");
                log_to_file("Exiting: --help flag");
                return;
            }
            _ => {
                log_to_file(&format!("Unknown arg: {}", args[1]));
            }
        }
    }

    log_to_file("Starting tokio runtime...");
    
    // Run async main
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main());
}

async fn async_main() {
    log_to_file("async_main started");

    // Try to load configuration - but don't fail startup if env vars missing
    let server = match create_server() {
        Ok(s) => {
            log_to_file("Server configured successfully");
            Some(s)
        },
        Err(e) => {
            log_to_file(&format!("Configuration incomplete: {}", e));
            None
        }
    };

    log_to_file("Starting stdio loop...");

    // Run async stdio message loop
    if let Err(e) = run_stdio_loop(server).await {
        log_to_file(&format!("Server error: {}", e));
    }
}

fn create_server() -> Result<D365McpServer, Box<dyn std::error::Error>> {
    use d365_odata_mcp::auth::{AuthConfig, AuthType, OAuth2Auth};
    
    let config = Config::load_default()?;
    let runtime_config = config.to_runtime()?;

    // Parse auth type
    let auth_type: AuthType = runtime_config.auth_type.parse()
        .unwrap_or(AuthType::AzureAd);

    log_to_file(&format!("Auth type: {:?}", auth_type));

    let auth_config = AuthConfig {
        auth_type,
        tenant_id: runtime_config.tenant_id.clone(),
        client_id: runtime_config.client_id.clone(),
        client_secret: runtime_config.client_secret.clone(),
        token_url: runtime_config.token_url.clone(),
        resource: runtime_config.resource.clone(),
    };

    let auth = Arc::new(OAuth2Auth::new(auth_config));

    let client = Arc::new(ODataClient::new(
        auth,
        runtime_config.endpoint.clone(),
        runtime_config.product.clone(),
        runtime_config.max_retries,
        runtime_config.retry_delay_ms,
    ));

    Ok(D365McpServer::new(client, Arc::new(runtime_config)))
}

async fn run_stdio_loop(server: Option<D365McpServer>) -> Result<(), std::io::Error> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    log_to_file("Waiting for input...");

    loop {
        line.clear();
        
        log_to_file("Reading line...");
        let bytes_read = reader.read_line(&mut line).await?;
        
        log_to_file(&format!("Read {} bytes: {:?}", bytes_read, line.trim()));
        
        if bytes_read == 0 {
            log_to_file("EOF received, shutting down");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            log_to_file("Empty line, skipping");
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(req) => {
                log_to_file(&format!("Parsed request: method={}, has_id={}", req.method, req.id.is_some()));
                req
            },
            Err(e) => {
                log_to_file(&format!("Parse error: {}", e));
                let error_response = JsonRpcResponse::error(None, -32700, &format!("Parse error: {}", e));
                let _ = send_response(&mut stdout, &error_response).await;
                continue;
            }
        };

        // Notifications don't have an id and should NOT receive a response
        let is_notification = request.id.is_none();
        
        if is_notification {
            log_to_file(&format!("Notification received: {}, no response needed", request.method));
            // Still process the notification but don't send response
            let _ = handle_request(&server, request).await;
            continue;
        }

        let response = handle_request(&server, request).await;
        log_to_file("Sending response...");
        let _ = send_response(&mut stdout, &response).await;
        log_to_file("Response sent");
    }

    Ok(())
}

async fn handle_request(server: &Option<D365McpServer>, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            log_to_file("Handling: initialize");
            let result = InitializeResult {
                protocol_version: "2024-11-05".to_string(),
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability {
                        list_changed: Some(false),
                    }),
                },
                server_info: ServerInfo {
                    name: "d365-odata-mcp".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            };
            JsonRpcResponse::success(id, serde_json::to_value(result).unwrap())
        }

        "initialized" | "notifications/initialized" => {
            log_to_file("Handling: initialized");
            JsonRpcResponse::success(id, serde_json::json!({}))
        }

        "tools/list" => {
            log_to_file("Handling: tools/list");
            let tools = match server {
                Some(s) => s.get_tools(),
                None => D365McpServer::get_tools_static(),
            };
            let result = ListToolsResult { tools };
            JsonRpcResponse::success(id, serde_json::to_value(result).unwrap())
        }

        "tools/call" => {
            log_to_file("Handling: tools/call");
            let server = match server {
                Some(s) => s,
                None => {
                    let result = CallToolResult::error(
                        "Server not configured. Missing environment variables: TENANT_ID, CLIENT_ID, CLIENT_SECRET, ENDPOINT".to_string()
                    );
                    return JsonRpcResponse::success(id, serde_json::to_value(result).unwrap());
                }
            };

            let params: CallToolParams = match request.params {
                Some(p) => match serde_json::from_value(p) {
                    Ok(params) => params,
                    Err(e) => {
                        return JsonRpcResponse::error(id, -32602, &format!("Invalid params: {}", e));
                    }
                },
                None => {
                    return JsonRpcResponse::error(id, -32602, "Missing params");
                }
            };

            let args = params.arguments.unwrap_or_default();
            let result: CallToolResult = server.call_tool(&params.name, &args).await;
            JsonRpcResponse::success(id, serde_json::to_value(result).unwrap())
        }

        "ping" => {
            log_to_file("Handling: ping");
            JsonRpcResponse::success(id, serde_json::json!({}))
        }

        method => {
            log_to_file(&format!("Unknown method: {}", method));
            JsonRpcResponse::success(id, serde_json::json!({}))
        }
    }
}

async fn send_response(stdout: &mut tokio::io::Stdout, response: &JsonRpcResponse) -> std::io::Result<()> {
    let json = serde_json::to_string(response).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    log_to_file(&format!("Response: {}", json));
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
