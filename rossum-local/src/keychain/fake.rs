use super::{Keychain, TokenEntry};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use ulid::Ulid;

#[derive(Default)]
pub struct InMemoryKeychain {
    tokens: Mutex<HashMap<Ulid, TokenEntry>>,
    creds: Mutex<HashMap<Ulid, (String, String)>>,
}

impl Keychain for InMemoryKeychain {
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()> {
        self.tokens.lock().unwrap().insert(id, entry.clone());
        Ok(())
    }

    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>> {
        Ok(self.tokens.lock().unwrap().get(&id).cloned())
    }

    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()> {
        self.creds
            .lock()
            .unwrap()
            .insert(id, (username.to_string(), password.to_string()));
        Ok(())
    }

    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>> {
        Ok(self.creds.lock().unwrap().get(&id).cloned())
    }

    fn delete_all(&self, id: Ulid) -> Result<()> {
        self.tokens.lock().unwrap().remove(&id);
        self.creds.lock().unwrap().remove(&id);
        Ok(())
    }
}
