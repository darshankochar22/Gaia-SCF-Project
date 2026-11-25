//! Where the full JSON record lives. Only its hash goes on chain.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub enum Storage {
    /// Files written to a folder and served by this service at /records/<hash>.json.
    Local { dir: PathBuf, public_base: String },
    /// Pinned to public IPFS through Pinata.
    Pinata { jwt: String, http: reqwest::Client },
}

impl Storage {
    pub fn from_env() -> Result<Self> {
        if let Ok(jwt) = std::env::var("PINATA_JWT") {
            if !jwt.is_empty() {
                return Ok(Self::Pinata { jwt, http: reqwest::Client::new() });
            }
        }
        let dir = PathBuf::from(std::env::var("RECORDS_DIR").unwrap_or_else(|_| "records".into()));
        std::fs::create_dir_all(&dir)?;
        let public_base = std::env::var("PUBLIC_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8787".into())
            .trim_end_matches('/')
            .to_string();
        Ok(Self::Local { dir, public_base })
    }

    pub fn records_dir(&self) -> Option<&PathBuf> {
        match self {
            Self::Local { dir, .. } => Some(dir),
            Self::Pinata { .. } => None,
        }
    }

    /// Stores the exact canonical bytes and returns the URI to anchor.
    pub async fn put(&self, hash_hex: &str, bytes: &[u8]) -> Result<String> {
        match self {
            Self::Local { dir, public_base } => {
                let path = dir.join(format!("{hash_hex}.json"));
                tokio::fs::write(&path, bytes).await.context("writing record")?;
                Ok(format!("{public_base}/records/{hash_hex}.json"))
            }
            Self::Pinata { jwt, http } => {
                let part = reqwest::multipart::Part::bytes(bytes.to_vec())
                    .file_name(format!("{hash_hex}.json"))
                    .mime_str("application/json")?;
                let form = reqwest::multipart::Form::new()
                    .part("file", part)
                    .text("network", "public");
                let res: serde_json::Value = http
                    .post("https://uploads.pinata.cloud/v3/files")
                    .bearer_auth(jwt)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let Some(cid) = res["data"]["cid"].as_str() else {
                    bail!("pinata response had no cid: {res}");
                };
                Ok(format!("ipfs://{cid}"))
            }
        }
    }
}
