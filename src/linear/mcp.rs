//! The Linear MCP server agents get, carrying the app's current OAuth token.

use crate::app::App;
use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;

/// Linear's hosted MCP with the app token, rewritten per turn so it carries the current token.
// ponytail: a turn longer than the 24h token lifetime loses Linear MCP mid-turn; proxy MCP through mothership if that ever happens.
pub fn config(app: &App, key: &str) -> Result<PathBuf> {
    let token = app.store.read(|s| s.linear.access_token.clone());
    let config = json!({ "mcpServers": { "linear": {
        "type": "http",
        "url": "https://mcp.linear.app/mcp",
        "headers": { "Authorization": format!("Bearer {token}") },
    }}});
    let dir = app.cfg.home.join("mcp");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", crate::store::file_name(key)));
    crate::store::write_private(&path, &serde_json::to_vec(&config)?)?;
    Ok(path)
}
