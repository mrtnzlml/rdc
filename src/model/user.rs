use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum user. Lightweight slice sufficient for the interactive
/// token_owner picker on `rdc deploy`; unknown fields survive via `extra`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct User {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Forward-compat: every field not modelled survives via round-trip.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl User {
    /// True iff the user is in the admin group (`/api/v1/groups/3`). The
    /// admin group is the canonical role for store-extension service
    /// accounts; the picker filters / ranks by this.
    pub fn is_admin(&self) -> bool {
        self.groups.iter().any(|g| g.ends_with("/groups/3"))
    }

    /// True iff this user looks like an auto-provisioned system account.
    /// Convention is `system_user__<hash>`; deleted variants get a
    /// `_deleted_*` suffix and are filtered out by `is_active=false`.
    pub fn is_system_user(&self) -> bool {
        self.username.starts_with("system_user__")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_minimal_response() {
        let payload = json!({
            "id": 938493,
            "url": "https://elis.rossum.ai/api/v1/users/938493",
            "username": "system_user__a556534d",
            "first_name": "SYSTEM USER",
            "last_name": "(DO NOT DELETE)",
            "is_active": true,
            "groups": ["https://elis.rossum.ai/api/v1/groups/3"]
        });
        let u: User = serde_json::from_value(payload).unwrap();
        assert_eq!(u.id, 938493);
        assert!(u.is_admin());
        assert!(u.is_system_user());
    }

    #[test]
    fn non_admin_user() {
        let payload = json!({
            "id": 1, "url": "u", "username": "alice",
            "is_active": true, "groups": ["https://x/api/v1/groups/2"]
        });
        let u: User = serde_json::from_value(payload).unwrap();
        assert!(!u.is_admin());
        assert!(!u.is_system_user());
    }
}
