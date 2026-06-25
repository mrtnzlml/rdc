use crate::support::client::LiveClient;
use anyhow::{ensure, Result};

/// Assert that the remote object of `kind`/`id` has `field` (dotted) equal to
/// `expected` (compared as JSON).
#[allow(dead_code)]
pub async fn assert_remote_field(
    client: &LiveClient,
    kind: &str,
    id: u64,
    field: &str,
    expected: &serde_json::Value,
) -> Result<()> {
    let v = client.get_value(kind, id).await?;
    let got = crate::support::assert_local::field(&v, field)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    ensure!(&got == expected, "remote {kind} {id} .{field}: got {got}, want {expected}");
    Ok(())
}

/// Assert a remote string field is a real API URL (resolution happened — not
/// a leftover `rdc://` portable ref).
#[allow(dead_code)]
pub async fn assert_remote_ref_resolved(
    client: &LiveClient,
    kind: &str,
    id: u64,
    field: &str,
) -> Result<()> {
    let v = client.get_value(kind, id).await?;
    let got = crate::support::assert_local::field(&v, field)
        .and_then(|x| x.as_str())
        .unwrap_or("");
    ensure!(
        got.starts_with("http") && !got.contains("rdc://"),
        "remote {kind} {id} .{field} not a resolved URL: {got:?}"
    );
    Ok(())
}
