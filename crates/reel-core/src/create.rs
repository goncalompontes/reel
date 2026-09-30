//! Creating `.torrent` files from local content.
//!
//! This is the other half of a self-hosted streaming setup: `reel create` turns
//! a file (or folder) you own into a torrent, which you can then seed with
//! `reel serve --add`. Nothing here touches the network.

use std::path::Path;

use anyhow::Context;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{CreateTorrentOptions, create_torrent};

/// A freshly created torrent, ready to be written to disk.
#[derive(Debug, Clone)]
pub struct CreatedTorrent {
    pub bytes: Vec<u8>,
    pub info_hash: String,
}

/// Build a single-file or multi-file torrent for `input`.
///
/// The file layout inside the torrent mirrors `input`: for a file, the torrent
/// name is the file name and the content lives at the torrent root; for a
/// directory, the torrent name is the directory name.
pub async fn create_torrent_file(
    input: &Path,
    name: Option<&str>,
    trackers: Vec<String>,
    piece_length: Option<u32>,
) -> anyhow::Result<CreatedTorrent> {
    if !input.exists() {
        anyhow::bail!("{} does not exist", input.display());
    }

    let options = CreateTorrentOptions {
        name,
        trackers,
        piece_length,
    };

    let spawner = BlockingSpawner::new(2);
    let result = create_torrent(input, options, &spawner)
        .await
        .with_context(|| format!("hashing {}", input.display()))?;

    let bytes = result.as_bytes().context("serialising torrent")?;
    Ok(CreatedTorrent {
        bytes: bytes.to_vec(),
        info_hash: result.info_hash().as_string(),
    })
}
