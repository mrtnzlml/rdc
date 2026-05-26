use super::{Keychain, TokenEntry};
use anyhow::{Context, Result};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use ulid::Ulid;

pub struct MacOsKeychain;

fn token_service() -> &'static str {
    crate::paths::keychain_service()
}

fn username_service() -> String {
    format!("{}.username", crate::paths::keychain_service())
}

fn password_service() -> String {
    format!("{}.password", crate::paths::keychain_service())
}

fn account(id: Ulid) -> String {
    id.to_string()
}

impl Keychain for MacOsKeychain {
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()> {
        let json = serde_json::to_vec(entry).context("serializing TokenEntry")?;
        set_generic_password(token_service(), &account(id), &json)
            .context("writing token to Keychain")?;
        Ok(())
    }

    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>> {
        match get_generic_password(token_service(), &account(id)) {
            Ok(bytes) => {
                let entry: TokenEntry = serde_json::from_slice(&bytes)
                    .context("parsing TokenEntry from Keychain")?;
                Ok(Some(entry))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e).context("reading token from Keychain"),
        }
    }

    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()> {
        set_generic_password(&username_service(), &account(id), username.as_bytes())
            .context("writing username to Keychain")?;
        set_generic_password(&password_service(), &account(id), password.as_bytes())
            .context("writing password to Keychain")?;
        Ok(())
    }

    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>> {
        let u = match get_generic_password(&username_service(), &account(id)) {
            Ok(bytes) => String::from_utf8(bytes).context("non-UTF-8 username in Keychain")?,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e).context("reading username from Keychain"),
        };
        let p = match get_generic_password(&password_service(), &account(id)) {
            Ok(bytes) => String::from_utf8(bytes).context("non-UTF-8 password in Keychain")?,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e).context("reading password from Keychain"),
        };
        Ok(Some((u, p)))
    }

    fn delete_all(&self, id: Ulid) -> Result<()> {
        for (service, label) in [
            (token_service().to_string(), "token"),
            (username_service(), "username"),
            (password_service(), "password"),
        ] {
            match delete_generic_password(&service, &account(id)) {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("deleting {label} from Keychain"));
                }
            }
        }
        Ok(())
    }
}

// Apple's errSecItemNotFound. Not re-exported by security-framework 2.x.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

fn is_not_found(e: &security_framework::base::Error) -> bool {
    e.code() == ERR_SEC_ITEM_NOT_FOUND
}
