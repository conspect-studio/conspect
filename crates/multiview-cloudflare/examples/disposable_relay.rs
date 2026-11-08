//! Explicit integration fixture. Reads secrets on stdin; writes response only to
//! a new private file. Never logs credentials. Caller owns cleanup by recorded UID.
use multiview_cloudflare::{Client, Credential};
use std::io::{Read, Write};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: serde_json::Value = serde_json::from_str(&input)?;
    let client = Client::new(
        v["account"].as_str().ok_or("account")?.into(),
        Credential::GlobalKey {
            email: v["email"].as_str().ok_or("email")?.into(),
            key: v["key"].as_str().ok_or("key")?.into(),
        },
    )?;
    if v["action"] == "delete" {
        client
            .delete(v["relay_id"].as_str().ok_or("relay")?)
            .await?;
        println!("deleted recorded disposable relay");
        return Ok(());
    }
    let path = v["output"].as_str().ok_or("output")?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let mut relay = if v["action"] == "recover-token" {
        client.get(v["relay_id"].as_str().ok_or("relay")?).await?
    } else {
        client
            .create("conspect-disposable-interop-20261108")
            .await?
    };
    if v["action"] == "recover-token" {
        let tokens = client
            .create_token(
                &relay.uid,
                &multiview_cloudflare::TokenRequest {
                    operations: vec![
                        multiview_cloudflare::Operation::Publish,
                        multiview_cloudflare::Operation::Subscribe,
                    ],
                    expires: None,
                    label: Some("disposable-interop-recovery".into()),
                },
            )
            .await?;
        relay.issuers = vec![multiview_cloudflare::Issuer {
            kind: "cloudflare_jwt".into(),
            cloudflare_tokens: tokens,
        }];
    }
    // Serialize secrets only at this explicit private-file handoff boundary.
    let tokens:Vec<_>=relay.issuers.iter().filter(|i|i.kind=="cloudflare_jwt").flat_map(|i|i.cloudflare_tokens.iter()).map(|t|serde_json::json!({"jti":t.jti,"operations":t.operations,"secret":t.secret.as_ref().map(|s|s.expose())})).collect();
    let data = serde_json::json!({"relay_id":relay.uid,"tokens":tokens});
    file.write_all(serde_json::to_string(&data)?.as_bytes())?;
    file.sync_all()?;
    println!(
        "created disposable relay {}; secrets saved privately",
        relay.uid
    );
    Ok(())
}
