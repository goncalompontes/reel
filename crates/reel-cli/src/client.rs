//! Thin HTTP client for a running `reel serve` (or the desktop app).

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, Response};
use reel_core::model::AddOutcome;
use reel_core::{TorrentView};

use crate::format;
use crate::{AddArgs, LsArgs, PlayArgs, RmArgs};

fn client() -> Client {
    Client::builder()
        .user_agent(concat!("reel-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("building http client")
}

fn endpoint(api: &str, path: &str) -> String {
    format!("{}{}", api.trim_end_matches('/'), path)
}

/// Turn a failed response into an error carrying the server's message.
fn check(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| format::truncate(body.trim(), 300));

    bail!("server returned {status}: {message}")
}

fn connect_error(api: &str, err: &reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "could not reach reel at {api} ({err}).\nIs `reel serve` running?"
    )
}

pub fn add(args: AddArgs) -> Result<()> {
    let http = client();
    let url = endpoint(&args.api, "/api/torrents");

    // A local .torrent file is uploaded as bytes; everything else is a URL the
    // server fetches itself.
    if std::path::Path::new(args.source.trim()).is_file() {
        let bytes = std::fs::read(args.source.trim())
            .with_context(|| format!("reading {}", args.source))?;
        let query = format!(
            "?media_only={}&paused={}&allow_overwrite={}",
            !args.all_files, args.paused, args.overwrite
        );

        eprintln!("uploading {} ({} bytes) ...", args.source, bytes.len());
        let response = http
            .post(endpoint(&args.api, &format!("/api/torrents{query}")))
            .header("content-type", "application/x-bittorrent")
            .body(bytes)
            .send()
            .map_err(|e| connect_error(&args.api, &e))?;

        return report_add(check(response)?, args.json);
    }

    let body = serde_json::json!({
        "source": args.source,
        "media_only": !args.all_files,
        "paused": args.paused,
        "allow_overwrite": args.overwrite,
        "upload_limit_bps": args.upload_limit,
        "download_limit_bps": args.download_limit,
        "initial_peers": args.peers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
    });

    eprintln!("adding {} ...", format::truncate(&args.source, 80));
    let response = http
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| connect_error(&args.api, &e))?;

    report_add(check(response)?, args.json)
}

fn report_add(response: Response, raw_json: bool) -> Result<()> {
    let text = response.text()?;

    if raw_json {
        println!("{text}");
        return Ok(());
    }

    let outcome: AddOutcome = serde_json::from_str(&text).context("decoding server response")?;
    let t = &outcome.torrent;
    let name = t.name.clone().unwrap_or_else(|| t.info_hash.clone());

    println!(
        "{} {name} (id {})",
        if outcome.was_new {
            "added"
        } else {
            "already known:"
        },
        t.id
    );

    match t.primary_file() {
        Some(f) => {
            println!("  playable   {} ({})", f.name, format::human_bytes(f.length));
            println!(
                "  stream     {}",
                f.stream.url.clone().unwrap_or_else(|| f.stream.path.clone())
            );
        }
        None => println!("  warning    no playable file detected; try --all-files"),
    }

    Ok(())
}

pub fn ls(args: LsArgs) -> Result<()> {
    let http = client();
    let url = endpoint(&args.api, "/api/torrents");

    let response = http
        .get(&url)
        .send()
        .map_err(|e| connect_error(&args.api, &e))?;
    let response = check(response)?;
    let text = response.text()?;

    if args.json {
        // Re-print through a serializer so the output is stable and pretty.
        let value: serde_json::Value = serde_json::from_str(&text)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let views: Vec<TorrentView> = serde_json::from_str(&text).context("decoding server response")?;
    println!("{}", format::torrent_table(&views));
    Ok(())
}

pub fn play(args: PlayArgs) -> Result<()> {
    let http = client();
    let url = endpoint(&args.api, &format!("/api/torrents/{}", args.id));

    let response = http
        .get(&url)
        .send()
        .map_err(|e| connect_error(&args.api, &e))?;
    let response = check(response)?;
    let view: TorrentView = response.json().context("decoding server response")?;

    let file = match args.file {
        Some(id) => view
            .files
            .iter()
            .find(|f| f.id == id)
            .ok_or_else(|| anyhow::anyhow!("torrent {} has no file {}", args.id, id))?,
        None => view.primary_file().ok_or_else(|| {
            anyhow::anyhow!(
                "torrent {} has no playable file; list files with `reel ls --json`",
                args.id
            )
        })?,
    };

    let stream_url = file
        .stream
        .url
        .clone()
        .unwrap_or_else(|| endpoint(&args.api, &file.stream.path));

    println!("{stream_url}");

    if args.print || args.player.trim().is_empty() {
        return Ok(());
    }

    let mut parts = args.player.split_whitespace();
    let program = parts.next().unwrap_or("mpv");
    let extra: Vec<&str> = parts.collect();

    let child = Command::new(program)
        .args(extra)
        .arg(&stream_url)
        .spawn()
        .with_context(|| format!("launching player `{program}`"))?;

    eprintln!(
        "launched {program} (pid {}) playing {} [{}]",
        child.id(),
        file.name,
        format::human_bytes(file.length)
    );
    Ok(())
}

pub fn rm(args: RmArgs) -> Result<()> {
    let http = client();
    let url = endpoint(
        &args.api,
        &format!("/api/torrents/{}?files={}", args.id, args.files),
    );

    let response = http
        .delete(&url)
        .send()
        .map_err(|e| connect_error(&args.api, &e))?;
    check(response)?;

    println!(
        "removed torrent {}{}",
        args.id,
        if args.files { " and its files" } else { "" }
    );
    Ok(())
}
