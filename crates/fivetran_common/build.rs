use std::{
    env,
    io::Result,
    path::{
        Path,
        PathBuf,
    },
};

use bytes::Bytes;
use futures_util::future::join_all;
use tokio::fs::{
    self,
    create_dir_all,
};

const REV: &str = "08da2f841be6042a410b0de6354025c44d5cf59a";

cfg_if::cfg_if! {
    if #[cfg(target_os = "macos")] {
        const PROTOC_BINARY_NAME: &str = "protoc-macos-universal";
    } else if #[cfg(all(target_os = "linux", target_arch = "aarch64"))] {
        const PROTOC_BINARY_NAME: &str = "protoc-linux-aarch64";
    } else if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
        const PROTOC_BINARY_NAME: &str = "protoc-linux-x86_64";
    } else {
        panic!("no protoc binary available for this architecture");
    }
}

fn set_protoc_path() {
    let root = Path::new("../pb_build/protoc");
    if root.exists() {
        let include_path = std::fs::canonicalize(root.join("include"))
            .expect("Failed to canonicalize protoc include path");
        std::env::set_var("PROTOC_INCLUDE", include_path);
        let binary_path = std::fs::canonicalize(root.join(PROTOC_BINARY_NAME))
            .expect("Failed to canonicalize protoc path");
        std::env::set_var("PROTOC", binary_path);
    }
}

async fn download_bytes_of_file(url: &str) -> anyhow::Result<Bytes> {
    Ok(reqwest::get(url).await?.bytes().await?)
}

async fn try_download_file(url: String, destination: &PathBuf) -> anyhow::Result<()> {
    let bytes = match download_bytes_of_file(&url).await {
        Ok(bytes) => bytes,
        Err(err) => {
            if destination.exists() {
                println!(
                    "cargo:warning=Could not download proto file from {url} ({err:?}). Proceeding \
                     with the existing proto file."
                );
                return Ok(());
            }
            anyhow::bail!(err);
        },
    };
    // Don't write to the file (and mark it as dirty) if it hasn't changed. Writing
    // to a watched file during a build script bumps its modification time,
    // which causes a subsequent `cargo build` to consider the file dirty.
    if destination.exists() {
        let existing_contents = fs::read(destination).await?;
        if existing_contents == bytes {
            return Ok(());
        }
    }
    fs::write(destination, bytes).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    set_protoc_path();

    let protos: &[&str] = &[
        "common.proto",
        "connector_sdk.proto",
        "destination_sdk.proto",
    ];
    let protos_dir = Path::join(Path::new(&env::var("OUT_DIR").unwrap()), "protos");
    create_dir_all(protos_dir.clone()).await?;

    let source_urls: Vec<String> = protos
        .iter()
        .map(|proto| {
            format!("https://raw.githubusercontent.com/fivetran/fivetran_sdk/{REV}/{proto}")
        })
        .collect();
    let destination_files: Vec<PathBuf> = protos
        .iter()
        .map(|proto| Path::join(&protos_dir, proto))
        .collect();

    let result = join_all(
        source_urls
            .into_iter()
            .zip(&destination_files)
            .map(|(source_url, destination_file)| try_download_file(source_url, destination_file)),
    )
    .await;
    for r in result {
        r.expect("Failed to download proto file");
    }

    tonic_build::configure()
        .btree_map(["."])
        .compile(&destination_files, &[protos_dir])?;

    Ok(())
}
