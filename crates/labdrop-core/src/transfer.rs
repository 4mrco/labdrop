use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sharing protocol associated with a client or transfer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolKind {
    Smb,
    WebDav,
    Http,
}

impl ProtocolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtocolKind::Smb => "SMB",
            ProtocolKind::WebDav => "WebDAV",
            ProtocolKind::Http => "HTTP",
        }
    }
}

/// Lifecycle state of an individual file transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferState {
    Active,
    Completed,
    Interrupted,
}

/// A tracked file transfer or SMB file access event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRecord {
    pub id: u64,
    pub protocol: ProtocolKind,
    pub client_ip: String,
    pub relative_path: String,
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
    pub exact_byte_progress: bool,
    pub state: TransferState,
    pub started_epoch_secs: u64,
}

/// Aggregated statistics for a connected destination computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSummary {
    pub client_ip: String,
    pub protocol: ProtocolKind,
    pub active_transfers: usize,
    pub completed_transfers: usize,
    pub bytes_transferred: u64,
    pub last_seen_epoch_secs: u64,
}

/// Snapshot of overall transfer visibility state for rendering in the TUI or CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferSnapshot {
    pub total_bytes_transferred: u64,
    pub active_transfer_count: usize,
    pub completed_transfer_count: usize,
    pub clients: Vec<ClientSummary>,
    pub recent_transfers: Vec<TransferRecord>,
}

/// Thread-safe tracker for active and completed file transfers across protocols.
#[derive(Debug)]
pub struct TransferTracker {
    next_id: AtomicU64,
    total_bytes: AtomicU64,
    inner: Mutex<TrackerInner>,
}

#[derive(Debug, Default)]
struct TrackerInner {
    transfers: Vec<TransferRecord>,
    clients: HashMap<String, ClientSummary>,
}

impl Default for TransferTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferTracker {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            total_bytes: AtomicU64::new(0),
            inner: Mutex::new(TrackerInner::default()),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Registers a client connection (e.g., when a client browses a directory or connects via SMB).
    pub fn record_client_activity(&self, client_ip: &str, protocol: ProtocolKind) {
        let now = now_epoch_secs();
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let entry = guard
            .clients
            .entry(client_ip.to_string())
            .or_insert_with(|| ClientSummary {
                client_ip: client_ip.to_string(),
                protocol,
                active_transfers: 0,
                completed_transfers: 0,
                bytes_transferred: 0,
                last_seen_epoch_secs: now,
            });
        entry.protocol = protocol;
        entry.last_seen_epoch_secs = now;
    }

    /// Starts tracking a new file transfer and returns its unique transfer ID.
    pub fn start_transfer(
        &self,
        protocol: ProtocolKind,
        client_ip: &str,
        relative_path: &str,
        total_bytes: Option<u64>,
        exact_byte_progress: bool,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let now = now_epoch_secs();
        let Ok(mut guard) = self.inner.lock() else {
            return id;
        };

        let client = guard
            .clients
            .entry(client_ip.to_string())
            .or_insert_with(|| ClientSummary {
                client_ip: client_ip.to_string(),
                protocol,
                active_transfers: 0,
                completed_transfers: 0,
                bytes_transferred: 0,
                last_seen_epoch_secs: now,
            });
        client.active_transfers += 1;
        client.last_seen_epoch_secs = now;

        guard.transfers.push(TransferRecord {
            id,
            protocol,
            client_ip: client_ip.to_string(),
            relative_path: relative_path.to_string(),
            bytes_transferred: 0,
            total_bytes,
            exact_byte_progress,
            state: TransferState::Active,
            started_epoch_secs: now,
        });

        // Keep history bounded to the most recent 100 transfers
        if guard.transfers.len() > 100 {
            guard.transfers.remove(0);
        }

        id
    }

    /// Adds `delta_bytes` to an active transfer's progress counter.
    pub fn add_bytes(&self, transfer_id: u64, delta_bytes: u64) {
        if delta_bytes == 0 {
            return;
        }
        self.total_bytes.fetch_add(delta_bytes, Ordering::Relaxed);
        let now = now_epoch_secs();
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };

        let mut client_ip_opt = None;
        for rec in guard.transfers.iter_mut().rev() {
            if rec.id == transfer_id {
                rec.bytes_transferred = rec.bytes_transferred.saturating_add(delta_bytes);
                client_ip_opt = Some(rec.client_ip.clone());
                break;
            }
        }
        if let Some(ip) = client_ip_opt {
            if let Some(client) = guard.clients.get_mut(&ip) {
                client.bytes_transferred = client.bytes_transferred.saturating_add(delta_bytes);
                client.last_seen_epoch_secs = now;
            }
        }
    }

    /// Marks a transfer as completed or interrupted.
    pub fn finish_transfer(&self, transfer_id: u64, completed: bool) {
        let now = now_epoch_secs();
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };

        let mut client_ip_opt = None;
        for rec in guard.transfers.iter_mut().rev() {
            if rec.id == transfer_id {
                if rec.state == TransferState::Active {
                    rec.state = if completed {
                        TransferState::Completed
                    } else {
                        TransferState::Interrupted
                    };
                    client_ip_opt = Some(rec.client_ip.clone());
                }
                break;
            }
        }
        if let Some(ip) = client_ip_opt {
            if let Some(client) = guard.clients.get_mut(&ip) {
                client.active_transfers = client.active_transfers.saturating_sub(1);
                if completed {
                    client.completed_transfers += 1;
                }
                client.last_seen_epoch_secs = now;
            }
        }
    }

    /// Produces a consistent snapshot of all clients and recent transfers.
    pub fn snapshot(&self) -> TransferSnapshot {
        let Ok(guard) = self.inner.lock() else {
            return TransferSnapshot::default();
        };

        let active_transfer_count = guard
            .transfers
            .iter()
            .filter(|t| t.state == TransferState::Active)
            .count();
        let completed_transfer_count = guard
            .transfers
            .iter()
            .filter(|t| t.state == TransferState::Completed)
            .count();

        let mut clients: Vec<ClientSummary> = guard.clients.values().cloned().collect();
        clients.sort_by_key(|a| std::cmp::Reverse(a.last_seen_epoch_secs));

        let mut recent_transfers = guard.transfers.clone();
        recent_transfers.reverse();

        TransferSnapshot {
            total_bytes_transferred: self.total_bytes.load(Ordering::Relaxed),
            active_transfer_count,
            completed_transfer_count,
            clients,
            recent_transfers,
        }
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
