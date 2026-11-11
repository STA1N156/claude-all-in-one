//! Kiro-Go compatible local prompt-cache accounting.
//!
//! The cache stores only SHA-256 prefix fingerprints and token counters. It is
//! shared across accounts and models, while the full-request checkpoint keeps
//! the model hash needed for exact reuse of the previous request's token count.
//! Only successful completions are written.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const CACHE_SHARD_COUNT: usize = 32;
const CACHE_ENTRIES_PER_SHARD: usize = 4096;
const CACHE_FILE_MAGIC: &[u8; 8] = b"KPCACHE1";
const CACHE_ENTRY_BYTES: usize = 80;
const MAX_HASH_JOBS: usize = 8;

#[derive(Debug, Clone, Copy)]
struct PromptCacheEntry {
    hash: [u8; 32],
    model: [u8; 32],
    updated_at: i64,
    tokens: i64,
}

#[derive(Debug, Clone, Copy)]
struct PromptCacheBreakpoint {
    fingerprint: [u8; 32],
    weight: u64,
}

#[derive(Debug, Default, Clone)]
struct ProfileState {
    ready: bool,
    breakpoints: Vec<PromptCacheBreakpoint>,
    total_weight: u64,
    model: [u8; 32],
    matched_weight: u64,
    observed_tokens: i64,
    exact: bool,
    total_tokens: u32,
}

/// Result handed to the billing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimResult {
    pub cache_read_tokens: u32,
    pub total_tokens: u32,
}

/// One request's prefix profile. Hashing runs in the background; the result is
/// consumed only when the response completes, so it does not delay first byte.
#[derive(Debug)]
pub struct PromptCacheProfile {
    state: Mutex<ProfileState>,
    ready: Condvar,
    max_hit_ratio: f64,
    committed: AtomicBool,
}

impl PromptCacheProfile {
    fn new(max_hit_ratio: f64) -> Self {
        Self {
            state: Mutex::new(ProfileState::default()),
            ready: Condvar::new(),
            max_hit_ratio,
            committed: AtomicBool::new(false),
        }
    }

    fn complete(&self, state: ProfileState) {
        let mut current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *current = state;
        current.ready = true;
        self.ready.notify_all();
    }

    fn wait(&self) -> ProfileState {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.ready {
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.clone()
    }

    /// Estimated input size used only when upstream sends no token usage.
    pub fn total_tokens(&self) -> u32 {
        self.wait().total_tokens
    }

    /// Calculate the cache hit for a successful request and warm the shared
    /// index. This mirrors Kiro-Go's `finish`.
    pub fn finish(&self, input_tokens: i32) -> SimResult {
        let state = self.wait();
        if input_tokens <= 0
            || !global().enabled()
            || !self
                .committed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return SimResult {
                cache_read_tokens: 0,
                total_tokens: state.total_tokens,
            };
        }

        let limit = (input_tokens as f64 * self.max_hit_ratio).floor() as i32;
        let cache_read = if state.exact {
            limit
        } else if state.observed_tokens > 0 {
            limit.min(state.observed_tokens.min(i32::MAX as i64) as i32)
        } else if state.total_weight == 0 {
            0
        } else {
            limit.min(
                ((input_tokens as i128 * state.matched_weight as i128)
                    / state.total_weight as i128) as i32,
            )
        }
        .clamp(0, input_tokens);

        global().record_success(&state, input_tokens);
        SimResult {
            cache_read_tokens: cache_read as u32,
            total_tokens: state.total_tokens,
        }
    }
}

struct PromptCacheTracker {
    shards: Vec<Mutex<HashMap<[u8; 32], PromptCacheEntry>>>,
    jobs: AtomicUsize,
    enabled: AtomicBool,
    ttl_secs: AtomicU64,
    per_shard_capacity: AtomicUsize,
    path: OnceLock<PathBuf>,
    persistence_started: AtomicBool,
}

impl PromptCacheTracker {
    fn new() -> Self {
        Self {
            shards: (0..CACHE_SHARD_COUNT)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            jobs: AtomicUsize::new(0),
            enabled: AtomicBool::new(true),
            ttl_secs: AtomicU64::new(300),
            per_shard_capacity: AtomicUsize::new(CACHE_ENTRIES_PER_SHARD),
            path: OnceLock::new(),
            persistence_started: AtomicBool::new(false),
        }
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.load(Ordering::Relaxed).max(1))
    }

    fn shard_capacity(&self) -> usize {
        self.per_shard_capacity.load(Ordering::Relaxed).max(1)
    }

    fn begin(&self, body: Value, model: &str, max_hit_ratio: f64) -> Option<Arc<PromptCacheProfile>> {
        if !self.enabled() {
            return None;
        }
        self.jobs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_HASH_JOBS).then_some(n + 1)
            })
            .ok()?;

        let profile = Arc::new(PromptCacheProfile::new(
            if max_hit_ratio.is_finite() {
                max_hit_ratio.clamp(0.0, 1.0)
            } else {
                0.9
            },
        ));
        let worker_profile = profile.clone();
        let model = model.to_string();
        let spawned = std::thread::Builder::new()
            .name("prompt-cache".into())
            .spawn(move || {
                struct JobGuard;
                impl Drop for JobGuard {
                    fn drop(&mut self) {
                        global().jobs.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _guard = JobGuard;
                let state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    build_and_lookup(&body, &model)
                }))
                .unwrap_or_default();
                worker_profile.complete(state);
            });
        if spawned.is_err() {
            self.jobs.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(profile)
    }

    fn lookup(&self, state: &mut ProfileState) {
        let now = unix_nanos();
        let cutoff = now.saturating_sub(self.ttl().as_nanos().min(i64::MAX as u128) as i64);
        for (index, point) in state.breakpoints.iter().enumerate().rev() {
            let shard = &self.shards[point.fingerprint[0] as usize % CACHE_SHARD_COUNT];
            let entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            let Some(entry) = entries.get(&point.fingerprint) else {
                continue;
            };
            if entry.updated_at <= cutoff {
                continue;
            }
            state.matched_weight = point.weight;
            state.exact = index + 1 == state.breakpoints.len();
            if entry.model == state.model {
                state.observed_tokens = entry.tokens;
            }
            return;
        }
    }

    fn record_success(&self, state: &ProfileState, input_tokens: i32) {
        let stamp = unix_nanos();
        let capacity = self.shard_capacity();
        for (index, point) in state.breakpoints.iter().enumerate() {
            let shard = &self.shards[point.fingerprint[0] as usize % CACHE_SHARD_COUNT];
            let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            if !entries.contains_key(&point.fingerprint) && entries.len() >= capacity {
                if let Some(key) = entries.keys().next().copied() {
                    entries.remove(&key);
                }
            }
            let entry = entries.entry(point.fingerprint).or_insert(PromptCacheEntry {
                hash: point.fingerprint,
                model: [0; 32],
                updated_at: stamp,
                tokens: 0,
            });
            entry.hash = point.fingerprint;
            entry.updated_at = stamp;
            if index + 1 == state.breakpoints.len() {
                entry.model = state.model;
                entry.tokens = input_tokens as i64;
            }
        }
    }

    fn load_index(&self, path: &Path) -> std::io::Result<()> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != CACHE_FILE_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown prompt cache format",
            ));
        }

        let now = unix_nanos();
        let cutoff =
            now.saturating_sub(self.ttl().as_nanos().min(i64::MAX as u128) as i64);
        let capacity = self.shard_capacity();
        let mut raw = [0u8; CACHE_ENTRY_BYTES];
        loop {
            match file.read_exact(&mut raw) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let entry = decode_entry(&raw);
            if entry.updated_at <= cutoff
                || entry.updated_at > now
                || entry.tokens < 0
            {
                continue;
            }
            let shard = &self.shards[entry.hash[0] as usize % CACHE_SHARD_COUNT];
            let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            if entries.len() < capacity {
                entries.insert(entry.hash, entry);
            }
        }
        Ok(())
    }

    fn save_index(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("bin.tmp");
        let mut file = fs::File::create(&temp)?;
        file.write_all(CACHE_FILE_MAGIC)?;

        let now = unix_nanos();
        let cutoff =
            now.saturating_sub(self.ttl().as_nanos().min(i64::MAX as u128) as i64);
        let mut buffer = Vec::with_capacity(CACHE_ENTRIES_PER_SHARD * CACHE_ENTRY_BYTES);
        for shard in &self.shards {
            let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            entries.retain(|_, entry| entry.updated_at > cutoff);
            buffer.clear();
            for entry in entries.values() {
                buffer.extend_from_slice(&encode_entry(*entry));
            }
            file.write_all(&buffer)?;
        }
        file.sync_all()?;
        drop(file);
        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                fs::remove_file(path)?;
                fs::rename(temp, path)
            }
            Err(e) => Err(e),
        }
    }

    fn start_persistence(&self, path: PathBuf) {
        if self.path.set(path.clone()).is_err()
            || self
                .persistence_started
                .swap(true, Ordering::AcqRel)
        {
            return;
        }
        if let Err(e) = self.load_index(&path) {
            tracing::warn!(error = %e, path = %path.display(), "prompt cache load failed");
        }
        std::thread::Builder::new()
            .name("prompt-cache-save".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                if let Err(e) = global().save_index(&path) {
                    tracing::warn!(error = %e, path = %path.display(), "prompt cache save failed");
                }
            })
            .ok();
    }
}

fn build_and_lookup(body: &Value, model: &str) -> ProfileState {
    let mut state = ProfileState {
        model: sha256(model.as_bytes()),
        total_tokens: crate::text_tokens::count_request_tokens(body).min(u32::MAX as u64) as u32,
        ..ProfileState::default()
    };
    let mut hasher = Sha256::new();
    let mut tool_ids = HashMap::<String, String>::new();

    add_value(&mut hasher, &mut state, "format", &json!("anthropic"));

    if let Some(tools) = body.get("tools").filter(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
        add_value(&mut hasher, &mut state, "tools", tools);
        checkpoint(&mut hasher, &mut state);
    }
    if let Some(system) = body.get("system").filter(|v| !v.is_null()) {
        let system = normalize_cache_content(system, &mut tool_ids);
        add_value(&mut hasher, &mut state, "system", &system);
        checkpoint(&mut hasher, &mut state);
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            let value = json!({
                "role": message.get("role").cloned().unwrap_or(Value::Null),
                "content": normalize_cache_content(
                    message.get("content").unwrap_or(&Value::Null),
                    &mut tool_ids,
                ),
            });
            add_value(&mut hasher, &mut state, "message", &value);
            checkpoint(&mut hasher, &mut state);
        }
    }

    global().lookup(&mut state);
    state
}

fn add_value(
    hasher: &mut Sha256,
    state: &mut ProfileState,
    kind: &str,
    value: &Value,
) {
    let data = serde_json::to_vec(value).unwrap_or_default();
    hasher.update(kind.as_bytes());
    hasher.update((data.len() as u64).to_be_bytes());
    hasher.update(&data);
    state.total_weight = state
        .total_weight
        .saturating_add((data.len() + kind.len() + 8) as u64);
}

fn checkpoint(hasher: &mut Sha256, state: &mut ProfileState) {
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(&hasher.clone().finalize());
    state.breakpoints.push(PromptCacheBreakpoint {
        fingerprint,
        weight: state.total_weight,
    });
}

fn normalize_cache_content(value: &Value, ids: &mut HashMap<String, String>) -> Value {
    let Some(blocks) = value.as_array() else {
        return value.clone();
    };
    let mut out = Vec::with_capacity(blocks.len());
    for item in blocks {
        let Some(block) = item.as_object() else {
            out.push(item.clone());
            continue;
        };
        let mut copy = block.clone();
        copy.remove("cache_control");
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use" | "server_tool_use") => {
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    copy.insert("id".into(), Value::String(cache_tool_id(ids, id)));
                }
            }
            Some("tool_result" | "web_search_tool_result") => {
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    copy.insert(
                        "tool_use_id".into(),
                        Value::String(cache_tool_id(ids, id)),
                    );
                }
            }
            _ => {}
        }
        out.push(Value::Object(copy));
    }
    Value::Array(out)
}

fn cache_tool_id(ids: &mut HashMap<String, String>, id: &str) -> String {
    if let Some(value) = ids.get(id) {
        return value.clone();
    }
    let value = format!("call_{}", ids.len() + 1);
    ids.insert(id.to_string(), value.clone());
    value
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn encode_entry(entry: PromptCacheEntry) -> [u8; CACHE_ENTRY_BYTES] {
    let mut out = [0u8; CACHE_ENTRY_BYTES];
    out[..32].copy_from_slice(&entry.hash);
    out[32..64].copy_from_slice(&entry.model);
    out[64..72].copy_from_slice(&entry.updated_at.to_le_bytes());
    out[72..80].copy_from_slice(&entry.tokens.to_le_bytes());
    out
}

fn decode_entry(raw: &[u8; CACHE_ENTRY_BYTES]) -> PromptCacheEntry {
    let mut hash = [0u8; 32];
    let mut model = [0u8; 32];
    hash.copy_from_slice(&raw[..32]);
    model.copy_from_slice(&raw[32..64]);
    PromptCacheEntry {
        hash,
        model,
        updated_at: i64::from_le_bytes(raw[64..72].try_into().unwrap()),
        tokens: i64::from_le_bytes(raw[72..80].try_into().unwrap()),
    }
}

fn global() -> &'static PromptCacheTracker {
    static TRACKER: OnceLock<PromptCacheTracker> = OnceLock::new();
    TRACKER.get_or_init(PromptCacheTracker::new)
}

/// Start a background Kiro-Go-style profile for an Anthropic request body.
pub fn begin_claude_profile(
    body: Value,
    model: &str,
    max_hit_ratio: f64,
) -> Option<Arc<PromptCacheProfile>> {
    global().begin(body, model, max_hit_ratio)
}

/// Enable or disable local cache-hit accounting.
pub fn set_enabled(enabled: bool) {
    global().enabled.store(enabled, Ordering::Relaxed);
}

/// Set the fingerprint lifetime. Existing values are pruned on lookup/save.
pub fn set_ttl_secs(secs: u64) {
    global().ttl_secs.store(secs.max(1), Ordering::Relaxed);
}

/// Set the per-shard capacity. Kiro-Go uses 32 shards x 4096 entries.
pub fn set_max_sessions(per_shard: usize) {
    global()
        .per_shard_capacity
        .store(per_shard.max(1), Ordering::Relaxed);
}

/// Load and periodically persist the fingerprint index.
pub fn configure_persistence(path: impl Into<PathBuf>) {
    global().start_persistence(path.into());
}
