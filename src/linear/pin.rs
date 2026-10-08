//! The one Linear workspace an instance serves: which workspace a token belongs to, and which
//! tokens and webhooks are let in once it is pinned.

use super::Linear;
use crate::app::App;
use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The workspace (Linear organization) an OAuth token belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub url_key: String,
    pub name: String,
}

pub(super) const VIEWER_ORGANIZATION: &str = "query { viewer { organization { id urlKey name } } }";

impl Workspace {
    pub(super) fn from_viewer(data: &Value) -> Result<Self> {
        let org = &data["viewer"]["organization"];
        let field = |k: &str| {
            org[k]
                .as_str()
                .map(String::from)
                .with_context(|| format!("viewer.organization.{k} missing"))
        };
        Ok(Self {
            id: field("id")?,
            url_key: field("urlKey")?,
            name: field("name")?,
        })
    }

    /// `LINEAR_WORKSPACE` names the workspace by ID or by URL key (`linear.app/<urlKey>`),
    /// the latter case-insensitively since Linear lowercases it.
    pub fn is(&self, setting: &str) -> bool {
        self.id == setting || self.url_key.eq_ignore_ascii_case(setting)
    }
}

impl std::fmt::Display for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {})", self.name, self.url_key, self.id)
    }
}

impl Linear {
    /// Pins `ws` unless `LINEAR_WORKSPACE` names another one, or a different workspace is
    /// already pinned: an instance never switches workspaces while running.
    pub(super) fn pin(&self, app: &App, ws: Workspace) -> Result<()> {
        let mut pinned = self.workspace.lock().expect("workspace lock poisoned");
        check_pin(app.cfg.linear.workspace.as_deref(), pinned.as_ref(), &ws)?;
        if pinned.is_none() {
            tracing::info!("Linear workspace: {ws}");
        }
        *pinned = Some(ws);
        Ok(())
    }

    pub(super) fn pinned_id(&self) -> Option<String> {
        self.workspace
            .lock()
            .expect("workspace lock poisoned")
            .as_ref()
            .map(|w| w.id.clone())
    }
}

/// Whether `ws` may be pinned, given the `LINEAR_WORKSPACE` setting and the workspace
/// already pinned, if any.
fn check_pin(want: Option<&str>, pinned: Option<&Workspace>, ws: &Workspace) -> Result<()> {
    if let Some(want) = want
        && !ws.is(want)
    {
        bail!("Linear token belongs to workspace {ws}, not LINEAR_WORKSPACE={want}");
    }
    if let Some(p) = pinned
        && p.id != ws.id
    {
        bail!("Linear workspace {p} is pinned; refusing {ws}");
    }
    Ok(())
}

/// Whether a webhook from `org` may be handled: only the pinned workspace's are. Before
/// anything is pinned, every webhook is refused because nothing says whose it is.
pub(super) fn admits(pinned: Option<&str>, org: Option<&str>) -> bool {
    pinned.is_some() && org == pinned
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_matches_id_or_url_key() {
        let data =
            json!({"viewer":{"organization":{"id":"org-1","urlKey":"alean","name":"ALEAN"}}});
        let ws = Workspace::from_viewer(&data).unwrap();
        assert!(ws.is("org-1"));
        assert!(ws.is("alean"));
        assert!(ws.is("ALEAN"), "URL key is case-insensitive");
        assert!(!ws.is("ORG-1"), "IDs are compared exactly");
        assert!(!ws.is("personal"));
        assert!(Workspace::from_viewer(&json!({"viewer":null})).is_err());
    }

    #[test]
    fn webhooks_only_from_the_pinned_workspace() {
        assert!(admits(Some("org-1"), Some("org-1")));
        assert!(!admits(Some("org-1"), Some("org-2")));
        assert!(!admits(Some("org-1"), None));
        assert!(!admits(None, Some("org-1")), "unpinned");
        assert!(!admits(None, None), "unpinned, no organization");
    }

    #[test]
    fn pin_checks_setting_and_existing_pin() {
        let ws = |id: &str, url_key: &str| Workspace {
            id: id.into(),
            url_key: url_key.into(),
            name: url_key.to_uppercase(),
        };
        let alean = ws("org-1", "alean");
        let personal = ws("org-2", "personal");
        assert!(
            check_pin(None, None, &alean).is_ok(),
            "first install, no setting"
        );
        assert!(check_pin(Some("alean"), None, &alean).is_ok());
        assert!(check_pin(Some("org-1"), None, &alean).is_ok());
        assert!(check_pin(Some("personal"), None, &alean).is_err());
        assert!(check_pin(None, Some(&alean), &alean).is_ok(), "same pin");
        assert!(check_pin(None, Some(&alean), &personal).is_err());
        assert!(
            check_pin(Some("personal"), Some(&alean), &personal).is_err(),
            "pin never switches"
        );
    }
}
