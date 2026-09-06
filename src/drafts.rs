//! Locally owned drafts and durable completion delivery. Presence is never a
//! verdict on a turn: only its producer (or recovery of that producer) closes it.

use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::Client;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    pid: u32,
    ready: bool,
    /// None means finalize the server's existing partial transcript. Once the
    /// final snapshot is acknowledged, clear it so retries cannot overwrite a
    /// later live-card edit to the delivered message.
    content: Option<String>,
    #[serde(skip)]
    delivering: bool,
}

pub struct Outbox {
    dir: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
}

impl Outbox {
    pub fn open(base: &str, username: &str) -> Result<Self> {
        let scope = format!("{base}\n{username}");
        let key = format!("{:x}", Sha256::digest(scope.as_bytes()));
        let dir = PathBuf::from(std::env::var("HOME").context("HOME is unset")?)
            .join(".mafold/drafts")
            .join(key);
        Self::load(dir, crate::platform::pid_alive)
    }

    fn load(dir: PathBuf, alive: impl Fn(u32) -> bool) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut entries = HashMap::new();
        for file in std::fs::read_dir(&dir)? {
            let path = file?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() {
                continue;
            }
            let mut entry: Entry = match std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|s| Ok(serde_json::from_slice(&s)?))
            {
                Ok(entry) => entry,
                Err(e) => {
                    eprintln!("draft journal {} unreadable: {e:#}", path.display());
                    continue;
                }
            };
            // Another daemon on this machine may own the same account. Its
            // drafts stay with it; never sweep a username's remote history.
            // An exec-based self-update keeps our PID. At startup no current
            // turns exist yet, so entries carrying our own PID are recoverable.
            if entry.pid != std::process::id() && alive(entry.pid) {
                continue;
            }
            entry.pid = std::process::id();
            entry.ready = true;
            entries.insert(id.to_string(), entry);
        }
        let outbox = Self {
            dir,
            entries: Mutex::new(entries),
        };
        for (id, entry) in outbox.entries.lock().unwrap().iter() {
            outbox.persist(id, entry)?;
        }
        Ok(outbox)
    }

    fn persist(&self, id: &str, entry: &Entry) -> Result<()> {
        uuid::Uuid::parse_str(id)?;
        let path = self.dir.join(format!("{id}.json"));
        let tmp = self.dir.join(format!("{id}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&tmp)?;
        serde_json::to_writer(&file, entry)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn track(&self, id: &str) -> Result<()> {
        let entry = Entry {
            pid: std::process::id(),
            ready: false,
            content: None,
            delivering: false,
        };
        let mut entries = self.entries.lock().unwrap();
        self.persist(id, &entry)?;
        entries.insert(id.to_string(), entry);
        Ok(())
    }

    pub fn complete(&self, id: &str, content: &str) -> Result<()> {
        let entry = Entry {
            pid: std::process::id(),
            ready: true,
            content: Some(content.into()),
            delivering: false,
        };
        let mut entries = self.entries.lock().unwrap();
        // Keep an in-memory retry even if the disk fills up; report the loss of
        // restart durability to the caller instead of claiming persistence.
        let saved = self.persist(id, &entry);
        entries.insert(id.to_string(), entry);
        saved
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        match std::fs::remove_file(self.dir.join(format!("{id}.json"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        entries.remove(id);
        Ok(())
    }

    pub async fn deliver(&self, client: &Client, id: &str) -> Result<bool> {
        let content = {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(id) else {
                return Ok(true);
            };
            if !entry.ready || entry.delivering {
                return Ok(false);
            }
            entry.delivering = true;
            entry.content.clone()
        };
        let result = async {
            if let Some(content) = content {
                client.edit_draft(id, &content).await?;
                let mut entries = self.entries.lock().unwrap();
                if let Some(entry) = entries.get_mut(id) {
                    entry.content = None;
                    self.persist(id, entry)?;
                }
            }
            client.finalize(id).await?;
            self.forget(id)?;
            Ok(true)
        }
        .await;
        if let Some(entry) = self.entries.lock().unwrap().get_mut(id) {
            entry.delivering = false;
        }
        result
    }

    pub async fn retry(&self, client: &Client) {
        let ids: Vec<String> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| e.ready && !e.delivering)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            match self.deliver(client, &id).await {
                Ok(true) => println!("→ recovered draft completion {id}"),
                Ok(false) => {}
                Err(e) => eprintln!("draft {id} completion still pending: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("mafold-drafts-{}", crate::session::device_id(None)))
    }

    #[test]
    fn restart_recovers_only_owned_drafts_from_dead_processes() {
        let dir = dir();
        let old = Outbox::load(dir.clone(), |_| false).unwrap();
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        old.track(a).unwrap();
        old.track(b).unwrap();
        old.complete(b, "the final answer").unwrap();
        // Simulate a different live daemon; this test process's own PID is
        // deliberately recoverable across an exec-based update.
        for (id, entry) in old.entries.lock().unwrap().iter_mut() {
            entry.pid = std::process::id() + 1;
            old.persist(id, entry).unwrap();
        }
        assert!(Outbox::load(dir.clone(), |_| true)
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .is_empty());
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        let entries = recovered.entries.lock().unwrap();
        assert!(entries[a].ready);
        assert!(entries[a].content.is_none());
        assert_eq!(entries[b].content.as_deref(), Some("the final answer"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exec_restart_recovers_entries_with_the_same_pid() {
        let dir = dir();
        let old = Outbox::load(dir.clone(), |_| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000005";
        old.track(id).unwrap();
        let recovered = Outbox::load(dir.clone(), |_| true).unwrap();
        assert!(recovered.entries.lock().unwrap()[id].ready);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_delivery_retains_final_snapshot_and_ignores_active_drafts() {
        let dir = dir();
        let outbox = Outbox::load(dir.clone(), |_| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000003";
        let client = Client::new("http://127.0.0.1:1".into(), "dev:test".into());
        outbox.track(id).unwrap();
        outbox.deliver(&client, id).await.unwrap(); // active: no network call
        outbox
            .complete(id, "complete text, no footer needed")
            .unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        assert!(!outbox.entries.lock().unwrap()[id].delivering);
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        assert_eq!(
            recovered.entries.lock().unwrap()[id].content.as_deref(),
            Some("complete text, no footer needed")
        );
        outbox.forget(id).unwrap();
        assert!(Outbox::load(dir.clone(), |_| false)
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn retry_after_snapshot_ack_only_finalizes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "dev:test".into(),
        );
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            for attempt in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len: usize = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::to_string)
                            })
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if request.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(request).unwrap();
                paths.push(
                    request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_string(),
                );
                // First finalize fails after the body was acknowledged. The
                // recovered entry must never re-send that body on its retry.
                let reply = if attempt == 1 {
                    r#"{"ok":false,"error_code":503,"description":"unavailable"}"#
                } else {
                    r#"{"ok":true,"result":{"ok":true}}"#
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            }
            paths
        });
        let dir = dir();
        let id = "00000000-0000-0000-0000-000000000004";
        let outbox = Outbox::load(dir.clone(), |_| false).unwrap();
        outbox.track(id).unwrap();
        outbox.complete(id, "full final answer").unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        assert!(recovered.entries.lock().unwrap()[id].content.is_none());
        assert!(recovered.deliver(&client, id).await.unwrap());
        assert!(recovered.entries.lock().unwrap().is_empty());
        assert_eq!(
            server.await.unwrap(),
            ["/api/botEditDraft", "/api/botFinalize", "/api/botFinalize"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
