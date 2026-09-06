use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{CallOptions, ConsumerOptions, RouteHandle, SubcConsumer};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};

use crate::config::SemanticBackendConfig;

const SYNAPSE_MODULE_ID: &str = "synapse";
const MODELS_LIST_OPERATION: &str = "models.list";
const QUERY_OPERATION: &str = "embed.query";
const BATCH_OPERATION: &str = "embed.batch";
const MAX_RESULT_PAGE_BYTES: usize = 512 * 1024;
const MAX_CALL_ATTEMPTS: usize = 4;
const MAX_NO_PROGRESS_POLLS: usize = 5;
const CIRCUIT_TIMEOUT_THRESHOLD: usize = 3;
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(5);
const RETRY_BACKOFF_MS: [u64; 3] = [100, 200, 400];
static LIVE_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynapseEmbeddingError {
    MissingConnectionFile,
    InvalidConnectionFile(String),
    MissingModel,
    DaemonUnavailable(String),
    CapabilityUnavailable(String),
    ModelUnavailable {
        requested: String,
        served: Vec<String>,
    },
    ModelNotCertified(String),
    InvalidEnvelope(String),
    ContentHashMismatch {
        id: String,
        expected: String,
        actual: String,
    },
    FingerprintMismatch {
        expected: String,
        served: String,
    },
    TableEpochMismatch {
        expected: u64,
        served: u64,
    },
    CircuitOpen,
    Timeout(String),
    NoProgress(String),
}

impl fmt::Display for SynapseEmbeddingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConnectionFile => write!(
                formatter,
                "synapse_missing_connection_file: semantic.backend=synapse requires user config subc.connection_file"
            ),
            Self::InvalidConnectionFile(path) => write!(
                formatter,
                "synapse_invalid_connection_file: subc.connection_file must be an absolute existing file: {path}"
            ),
            Self::MissingModel => write!(
                formatter,
                "synapse_missing_model: semantic.backend=synapse requires semantic.model"
            ),
            Self::DaemonUnavailable(error) => {
                write!(formatter, "synapse_daemon_unavailable: {error}")
            }
            Self::CapabilityUnavailable(error) => {
                write!(formatter, "synapse_capability_unavailable: {error}")
            }
            Self::ModelUnavailable { requested, served } => write!(
                formatter,
                "synapse_model_unavailable: requested {requested}; served models: {}",
                served.join(", ")
            ),
            Self::ModelNotCertified(model) => {
                write!(formatter, "synapse_model_not_certified: {model}")
            }
            Self::InvalidEnvelope(error) => {
                write!(formatter, "synapse_invalid_envelope: {error}")
            }
            Self::ContentHashMismatch { id, expected, actual } => write!(
                formatter,
                "synapse_content_sha256_mismatch: item {id} expected {expected}, received {actual}"
            ),
            Self::FingerprintMismatch { expected, served } => write!(
                formatter,
                "synapse_fingerprint_mismatch: expected {expected}, served {served}"
            ),
            Self::TableEpochMismatch { expected, served } => write!(
                formatter,
                "synapse_table_epoch_mismatch: expected {expected}, served {served}"
            ),
            Self::CircuitOpen => write!(
                formatter,
                "synapse_circuit_open: repeated daemon timeouts temporarily paused embedding calls"
            ),
            Self::Timeout(operation) => {
                write!(formatter, "synapse_timeout: {operation} exceeded its deadline")
            }
            Self::NoProgress(job) => write!(
                formatter,
                "synapse_batch_no_progress: job {job} returned no new chunks repeatedly"
            ),
        }
    }
}

impl std::error::Error for SynapseEmbeddingError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseIdentity {
    pub fingerprint: String,
    pub table_epoch: u64,
    pub equivalent_to: Vec<String>,
}

impl SynapseIdentity {
    pub fn accepts(&self, fingerprint: &str) -> bool {
        fingerprint == self.fingerprint
            || self.equivalent_to.iter().any(|alias| alias == fingerprint)
    }

    fn observe_equivalence(&mut self, served: &str, aliases: &[String]) -> bool {
        let connected = self.accepts(served)
            || aliases.iter().any(|alias| self.accepts(alias))
            || aliases.iter().any(|alias| alias == &self.fingerprint);
        if !connected {
            return false;
        }
        let mut class = HashSet::from([self.fingerprint.as_str(), served]);
        class.extend(self.equivalent_to.iter().map(String::as_str));
        class.extend(aliases.iter().map(String::as_str));
        self.equivalent_to = class
            .into_iter()
            .filter(|candidate| *candidate != self.fingerprint)
            .map(str::to_string)
            .collect();
        self.equivalent_to.sort();
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseModelMetadata {
    pub model: String,
    pub dims: Option<usize>,
    pub recommended_rows: usize,
    pub recommended_token_budget: usize,
    pub certified: bool,
    pub identity: SynapseIdentity,
}

#[derive(Debug, Clone)]
struct BatchItem {
    id: String,
    text: String,
    content_sha256: String,
}

struct SynapseState {
    connection_file: PathBuf,
    route_project_root: PathBuf,
    route_harness: String,
    model: String,
    call_timeout: Duration,
    consumer: Option<SubcConsumer>,
    route: Option<RouteHandle>,
    consecutive_timeouts: usize,
    circuit_open_until: Option<Instant>,
}

pub struct SynapseEmbeddingClient {
    runtime: tokio::runtime::Runtime,
    state: SynapseState,
    metadata: SynapseModelMetadata,
    models_list_envelope: Vec<u8>,
}

impl SynapseEmbeddingClient {
    pub fn from_config(config: &SemanticBackendConfig) -> Result<Self, SynapseEmbeddingError> {
        let connection_file = config
            .subc_connection_file
            .clone()
            .ok_or(SynapseEmbeddingError::MissingConnectionFile)?;
        validate_connection_file(&connection_file)?;
        let model = config.model.trim();
        if model.is_empty() {
            return Err(SynapseEmbeddingError::MissingModel);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))?;
        let mut state = SynapseState {
            connection_file,
            route_project_root: config
                .route_project_root
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from(".")),
            route_harness: config
                .route_harness
                .clone()
                .unwrap_or_else(|| "aft".to_string()),
            model: model.to_string(),
            call_timeout: Duration::from_millis(config.timeout_ms.max(1)),
            consumer: None,
            route: None,
            consecutive_timeouts: 0,
            circuit_open_until: None,
        };
        let (metadata, models_list_envelope) = runtime.block_on(state.discover_model())?;
        Ok(Self {
            runtime,
            state,
            metadata,
            models_list_envelope,
        })
    }

    pub fn metadata(&self) -> &SynapseModelMetadata {
        &self.metadata
    }

    pub fn identity(&self) -> &SynapseIdentity {
        &self.metadata.identity
    }

    /// Raw successful discovery envelope, exposed for the gated live fixture probe.
    pub fn models_list_envelope(&self) -> &[u8] {
        &self.models_list_envelope
    }

    pub fn probe_dimension(&mut self, timeout: Duration) -> Result<usize, SynapseEmbeddingError> {
        let vector = self.embed_query("semantic index fingerprint probe", timeout)?;
        Ok(vector.len())
    }

    pub fn embed_query(
        &mut self,
        text: &str,
        timeout: Duration,
    ) -> Result<Vec<f32>, SynapseEmbeddingError> {
        let item = BatchItem {
            id: "query:0".to_string(),
            text: text.to_string(),
            content_sha256: content_sha256(text),
        };
        let params = constrained_params(
            &self.state.model,
            &self.metadata.identity,
            json!({
                "id": item.id,
                "text": item.text,
                "content_sha256": item.content_sha256,
                "deadline_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }),
        );
        let raw = self
            .runtime
            .block_on(self.state.call(QUERY_OPERATION, params, timeout))?;
        capture_live_envelope(QUERY_OPERATION, &raw);
        let parsed = parse_embedding_page(&raw, &[item.clone()])?;
        self.validate_page_identity(&parsed)?;
        let vector = parsed
            .vectors
            .get(&item.id)
            .or_else(|| parsed.vectors.values().next())
            .cloned()
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(
                    "embed.query response returned no vector".to_string(),
                )
            })?;
        self.validate_dimension(&vector)?;
        Ok(vector)
    }

    pub fn embed_batch(
        &mut self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, SynapseEmbeddingError> {
        let items = texts
            .iter()
            .enumerate()
            .map(|(index, text)| BatchItem {
                id: format!("item:{index}"),
                text: text.clone(),
                content_sha256: content_sha256(text),
            })
            .collect::<Vec<_>>();
        let pages = split_batch_pages(
            &items,
            self.metadata.recommended_rows,
            self.metadata.recommended_token_budget,
        );
        let mut vectors = HashMap::with_capacity(items.len());
        for page in pages {
            vectors.extend(self.embed_batch_page(&page)?);
        }
        items
            .iter()
            .map(|item| {
                vectors.remove(&item.id).ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope(format!(
                        "embed.batch response omitted item {}",
                        item.id
                    ))
                })
            })
            .collect()
    }

    fn embed_batch_page(
        &mut self,
        items: &[BatchItem],
    ) -> Result<HashMap<String, Vec<f32>>, SynapseEmbeddingError> {
        let request_key = batch_request_key(&self.state.model, &self.metadata.identity, items);
        let wire_items = items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "text": item.text,
                    "content_sha256": item.content_sha256,
                })
            })
            .collect::<Vec<_>>();
        let submit = constrained_params(
            &self.state.model,
            &self.metadata.identity,
            json!({ "items": wire_items, "request_key": request_key }),
        );
        let timeout = self.state.call_timeout;
        let raw = self
            .runtime
            .block_on(self.state.call(BATCH_OPERATION, submit, timeout))?;
        capture_live_envelope(BATCH_OPERATION, &raw);
        let mut page = parse_embedding_page(&raw, items)?;
        self.validate_page_identity(&page)?;
        let Some(job_id) = page.job_id.clone() else {
            self.validate_vectors(&page.vectors)?;
            return Ok(page.vectors);
        };

        let deadline = Instant::now() + timeout;
        let mut vectors = HashMap::new();
        vectors.extend(page.vectors.drain());
        let mut next_chunk = page.next_chunk_id.clone();
        let mut last_progress = (vectors.len(), next_chunk.clone());
        let mut no_progress_polls = 0usize;
        loop {
            if page.done && next_chunk.is_none() {
                break;
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| SynapseEmbeddingError::Timeout(BATCH_OPERATION.to_string()))?;
            let mut continuation = Map::new();
            continuation.insert("job_id".to_string(), Value::String(job_id.clone()));
            continuation.insert(
                "request_key".to_string(),
                Value::String(request_key.clone()),
            );
            if let Some(chunk_id) = next_chunk.clone() {
                continuation.insert("chunk_id".to_string(), Value::String(chunk_id));
            }
            let poll = constrained_params(
                &self.state.model,
                &self.metadata.identity,
                Value::Object(continuation),
            );
            let raw = self
                .runtime
                .block_on(self.state.call(BATCH_OPERATION, poll, remaining))?;
            capture_live_envelope(BATCH_OPERATION, &raw);
            page = parse_embedding_page(&raw, items)?;
            self.validate_page_identity(&page)?;
            vectors.extend(page.vectors.drain());
            next_chunk = page.next_chunk_id.clone();
            let progress = (vectors.len(), next_chunk.clone());
            if progress == last_progress {
                no_progress_polls += 1;
                if no_progress_polls >= MAX_NO_PROGRESS_POLLS {
                    return Err(SynapseEmbeddingError::NoProgress(job_id));
                }
                std::thread::sleep(Duration::from_millis(100));
            } else {
                no_progress_polls = 0;
                last_progress = progress;
            }
            if page.done && next_chunk.is_none() {
                break;
            }
        }
        self.validate_vectors(&vectors)?;
        Ok(vectors)
    }

    fn validate_page_identity(
        &mut self,
        page: &ParsedEmbeddingPage,
    ) -> Result<(), SynapseEmbeddingError> {
        if page.table_epoch != self.metadata.identity.table_epoch {
            return Err(SynapseEmbeddingError::TableEpochMismatch {
                expected: self.metadata.identity.table_epoch,
                served: page.table_epoch,
            });
        }
        if !self
            .metadata
            .identity
            .observe_equivalence(&page.fingerprint, &page.equivalent_to)
            || !self.metadata.identity.accepts(&page.fingerprint)
        {
            return Err(SynapseEmbeddingError::FingerprintMismatch {
                expected: self.metadata.identity.fingerprint.clone(),
                served: page.fingerprint.clone(),
            });
        }
        Ok(())
    }

    fn validate_vectors(
        &mut self,
        vectors: &HashMap<String, Vec<f32>>,
    ) -> Result<(), SynapseEmbeddingError> {
        for vector in vectors.values() {
            self.validate_dimension(vector)?;
        }
        Ok(())
    }

    fn validate_dimension(&mut self, vector: &[f32]) -> Result<(), SynapseEmbeddingError> {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return Err(SynapseEmbeddingError::InvalidEnvelope(
                "embedding vector must contain finite values".to_string(),
            ));
        }
        match self.metadata.dims {
            Some(dims) if dims != vector.len() => {
                Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                    "embedding dimension mismatch: catalog={dims}, response={}",
                    vector.len()
                )))
            }
            None => {
                self.metadata.dims = Some(vector.len());
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }
}

impl SynapseState {
    async fn discover_model(
        &mut self,
    ) -> Result<(SynapseModelMetadata, Vec<u8>), SynapseEmbeddingError> {
        let timeout = self.call_timeout.min(Duration::from_secs(3));
        let raw = self.call(MODELS_LIST_OPERATION, json!({}), timeout).await?;
        let models = parse_models_list(&raw)?;
        if let Some(model) = models.iter().find(|model| model.model == self.model) {
            if !model.certified {
                return Err(SynapseEmbeddingError::ModelNotCertified(self.model.clone()));
            }
            if model.recommended_rows == 0 || model.recommended_token_budget == 0 {
                return Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                    "configured model {} has no usable recommended_batch rows/token_budget",
                    self.model
                )));
            }
            return Ok((model.clone(), raw));
        }
        let mut served = models
            .into_iter()
            .map(|model| model.model)
            .collect::<Vec<_>>();
        served.sort();
        Err(SynapseEmbeddingError::ModelUnavailable {
            requested: self.model.clone(),
            served,
        })
    }

    async fn call(
        &mut self,
        operation: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Vec<u8>, SynapseEmbeddingError> {
        if self
            .circuit_open_until
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(SynapseEmbeddingError::CircuitOpen);
        }
        self.circuit_open_until = None;
        let body = serde_json::to_vec(&json!({ "method": operation, "params": params }))
            .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
        let mut last_error = String::new();
        for attempt in 0..MAX_CALL_ATTEMPTS {
            if let Err(error) = self.ensure_route().await {
                last_error = error.to_string();
            } else {
                let request = self
                    .consumer
                    .as_ref()
                    .expect("consumer exists when route exists")
                    .request(
                        self.route.as_ref().expect("route ensured"),
                        body.clone(),
                        CallOptions::default(),
                    );
                match tokio::time::timeout(timeout, request).await {
                    Ok(Ok(response)) => {
                        self.consecutive_timeouts = 0;
                        if response.len() > MAX_RESULT_PAGE_BYTES {
                            return Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                                "{operation} response exceeded 512KiB page bound"
                            )));
                        }
                        return decode_result_envelope(response);
                    }
                    Ok(Err(error)) => {
                        last_error = error.to_string();
                        self.reset_connection();
                    }
                    Err(_) => {
                        last_error = format!("{operation} timed out");
                        self.note_timeout();
                        self.reset_connection();
                        if self.circuit_open_until.is_some() {
                            return Err(SynapseEmbeddingError::CircuitOpen);
                        }
                    }
                }
            }
            if attempt + 1 < MAX_CALL_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS[attempt.min(2)])).await;
            }
        }
        if last_error.to_ascii_lowercase().contains("timed out")
            || last_error.to_ascii_lowercase().contains("timeout")
        {
            Err(SynapseEmbeddingError::Timeout(operation.to_string()))
        } else {
            Err(SynapseEmbeddingError::DaemonUnavailable(last_error))
        }
    }

    async fn ensure_route(&mut self) -> Result<(), SynapseEmbeddingError> {
        if self.consumer.is_some() && self.route.is_some() {
            return Ok(());
        }
        let options = ConsumerOptions {
            call_timeout: self.call_timeout,
            ..ConsumerOptions::default()
        };
        let consumer = SubcConsumer::connect(&self.connection_file, options)
            .await
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))?;
        let catalog = consumer
            .catalog_list()
            .await
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))?;
        if !catalog_advertises_synapse(&catalog.modules) {
            return Err(SynapseEmbeddingError::CapabilityUnavailable(
                "synapse management surface does not advertise models.list, embed.query, and embed.batch"
                    .to_string(),
            ));
        }
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface {
                    module_id: SYNAPSE_MODULE_ID.to_string(),
                },
                BindIdentity {
                    project_root: self
                        .route_project_root
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    harness: self.route_harness.clone(),
                    session: format!("aft-semantic-{}", std::process::id()),
                },
                CallOptions::default(),
            )
            .await
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))?;
        self.consumer = Some(consumer);
        self.route = Some(route);
        Ok(())
    }

    fn reset_connection(&mut self) {
        self.route = None;
        self.consumer = None;
    }

    fn note_timeout(&mut self) {
        self.consecutive_timeouts += 1;
        if self.consecutive_timeouts >= CIRCUIT_TIMEOUT_THRESHOLD {
            self.circuit_open_until = Some(Instant::now() + CIRCUIT_COOLDOWN);
        }
    }
}

fn capture_live_envelope(operation: &str, raw: &[u8]) {
    let Some(directory) = capture_directory_with(|name| std::env::var_os(name)) else {
        return;
    };
    let sequence = LIVE_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let operation = operation.replace('.', "-");
    let _ = std::fs::write(
        directory.join(format!("{operation}-{sequence}-live.json")),
        raw,
    );
}

fn capture_directory_with(
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    lookup("AFT_SYNAPSE_CAPTURE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn validate_connection_file(path: &Path) -> Result<(), SynapseEmbeddingError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(SynapseEmbeddingError::InvalidConnectionFile(
            path.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

fn catalog_advertises_synapse(entries: &[subc_client_rs::CatalogEntry]) -> bool {
    entries.iter().any(|entry| {
        entry.module_id == SYNAPSE_MODULE_ID
            && entry.roles.iter().any(|role| {
                matches!(
                    role,
                    ProviderRole::ManagementSurface { operations, .. }
                        if [MODELS_LIST_OPERATION, QUERY_OPERATION, BATCH_OPERATION]
                            .iter()
                            .all(|required| operations.iter().any(|operation| operation.name == *required))
                )
            })
    })
}

fn decode_result_envelope(response: Vec<u8>) -> Result<Vec<u8>, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(&response)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let result = value.get("result").cloned().unwrap_or(value);
    if let Some(error) = result.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Synapse returned an unspecified error");
        return match code {
            "not_certified" => Err(SynapseEmbeddingError::ModelNotCertified(
                message.to_string(),
            )),
            "substitution_rejected" => Err(SynapseEmbeddingError::FingerprintMismatch {
                expected: "required fingerprint".to_string(),
                served: message.to_string(),
            }),
            _ => Err(SynapseEmbeddingError::DaemonUnavailable(format!(
                "{code}: {message}"
            ))),
        };
    }
    serde_json::to_vec(&result)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))
}

fn parse_models_list(raw: &[u8]) -> Result<Vec<SynapseModelMetadata>, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let envelope = value.get("result").unwrap_or(&value);
    let table_epoch = integer_field(envelope, &["table_epoch", "tableEpoch"]);
    let entries = if let Some(entries) = envelope.get("models").and_then(Value::as_array) {
        entries
    } else if let Some(entries) = envelope.get("entries").and_then(Value::as_array) {
        entries
    } else if let Some(entries) = envelope.as_array() {
        entries
    } else {
        return Err(SynapseEmbeddingError::InvalidEnvelope(
            "models.list response has no models array".to_string(),
        ));
    };
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let model = string_field(entry, &["model", "model_id"]).ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("model entry has no id".to_string())
        })?;
        let certified = entry.get("certified").and_then(Value::as_bool) != Some(false)
            && string_field(entry, &["status", "state"]).as_deref() != Some("not_certified");
        let fingerprint = string_field(entry, &["fingerprint"])
            .or_else(|| {
                entry
                    .get("fingerprints")
                    .and_then(Value::as_array)
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("model {model} has no fingerprint"))
            })?;
        let model_epoch = integer_field(entry, &["table_epoch", "tableEpoch"])
            .or(table_epoch)
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("model {model} has no table_epoch"))
            })?;
        let dims = integer_field(entry, &["dims", "dimensions"])
            .and_then(|value| usize::try_from(value).ok());
        let (recommended_rows, recommended_token_budget) = entry
            .get("recommended_batch")
            .or_else(|| entry.get("recommendedBatch"))
            .and_then(parse_recommended_batch)
            .unwrap_or((0, 0));
        models.push(SynapseModelMetadata {
            model,
            dims,
            recommended_rows,
            recommended_token_budget,
            certified,
            identity: SynapseIdentity {
                fingerprint,
                table_epoch: model_epoch,
                equivalent_to: string_array_field(entry, "equivalent_to"),
            },
        });
    }
    Ok(models)
}

fn parse_recommended_batch(value: &Value) -> Option<(usize, usize)> {
    let object = value.as_object()?;
    let rows = object
        .get("rows")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let token_budget = object
        .get("token_budget")
        .or_else(|| object.get("tokenBudget"))?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    (rows > 0 && token_budget > 0).then_some((rows, token_budget))
}

#[derive(Debug)]
struct ParsedEmbeddingPage {
    vectors: HashMap<String, Vec<f32>>,
    fingerprint: String,
    table_epoch: u64,
    equivalent_to: Vec<String>,
    job_id: Option<String>,
    next_chunk_id: Option<String>,
    done: bool,
}

fn parse_embedding_page(
    raw: &[u8],
    expected_items: &[BatchItem],
) -> Result<ParsedEmbeddingPage, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let envelope = value.get("result").unwrap_or(&value);
    let fingerprint =
        string_field(envelope, &["fingerprint", "served_fingerprint"]).ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("response has no fingerprint".to_string())
        })?;
    let table_epoch = integer_field(envelope, &["table_epoch", "tableEpoch"]).ok_or_else(|| {
        SynapseEmbeddingError::InvalidEnvelope("response has no table_epoch".to_string())
    })?;
    let expected = expected_items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<HashMap<_, _>>();
    let mut vectors = HashMap::new();
    if let Some(items) = envelope
        .get("vectors")
        .or_else(|| envelope.get("items"))
        .or_else(|| envelope.get("results"))
        .and_then(Value::as_array)
    {
        for (index, item) in items.iter().enumerate() {
            let id = string_field(item, &["id"])
                .or_else(|| expected_items.get(index).map(|item| item.id.clone()))
                .ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope("vector item has no id".to_string())
                })?;
            let expected_item = expected.get(id.as_str()).ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("response returned unknown id {id}"))
            })?;
            verify_content_hash(item, expected_item)?;
            vectors.insert(id, parse_vector(item)?);
        }
    } else if envelope.get("vector").is_some() || envelope.get("embedding").is_some() {
        let expected_item = expected_items.first().ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("unexpected unkeyed vector".to_string())
        })?;
        verify_content_hash(envelope, expected_item)?;
        vectors.insert(expected_item.id.clone(), parse_vector(envelope)?);
    }
    let next_chunk_id = string_field(
        envelope,
        &["next_chunk_id", "nextChunkId", "next_cursor", "cursor"],
    );
    let done = envelope.get("done").and_then(Value::as_bool) == Some(true)
        || envelope.get("complete").and_then(Value::as_bool) == Some(true)
        || (envelope.get("job_id").is_some() && next_chunk_id.is_none() && !vectors.is_empty());
    Ok(ParsedEmbeddingPage {
        vectors,
        fingerprint,
        table_epoch,
        equivalent_to: string_array_field(envelope, "equivalent_to"),
        job_id: string_field(envelope, &["job_id", "jobId"]),
        next_chunk_id,
        done,
    })
}

fn parse_vector(value: &Value) -> Result<Vec<f32>, SynapseEmbeddingError> {
    let values = value
        .get("vector")
        .or_else(|| value.get("embedding"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("vector item has no vector".to_string())
        })?;
    values
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .map(|number| number as f32)
                .ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope(
                        "embedding vector contains a non-finite value".to_string(),
                    )
                })
        })
        .collect()
}

fn verify_content_hash(
    response: &Value,
    expected: &BatchItem,
) -> Result<(), SynapseEmbeddingError> {
    let actual = response
        .get("content_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope(format!(
                "response item {} omitted content_sha256",
                expected.id
            ))
        })?;
    if actual != expected.content_sha256 {
        return Err(SynapseEmbeddingError::ContentHashMismatch {
            id: expected.id.clone(),
            expected: expected.content_sha256.clone(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

fn constrained_params(model: &str, identity: &SynapseIdentity, extra: Value) -> Value {
    let mut params = Map::new();
    params.insert("model".to_string(), Value::String(model.to_string()));
    params.insert(
        "required_fingerprint".to_string(),
        Value::String(identity.fingerprint.clone()),
    );
    params.insert("required_epoch".to_string(), json!(identity.table_epoch));
    params.insert("allow_equivalent".to_string(), Value::Bool(true));
    params.insert("accept_declared".to_string(), Value::Bool(false));
    if let Some(extra) = extra.as_object() {
        params.extend(extra.clone());
    }
    Value::Object(params)
}

fn batch_request_key(model: &str, identity: &SynapseIdentity, items: &[BatchItem]) -> String {
    let mut stable = BTreeMap::new();
    stable.insert("accept_declared", json!(false));
    stable.insert("allow_equivalent", json!(true));
    stable.insert(
        "content_sha256",
        json!(items
            .iter()
            .map(|item| &item.content_sha256)
            .collect::<Vec<_>>()),
    );
    stable.insert(
        "ids",
        json!(items.iter().map(|item| &item.id).collect::<Vec<_>>()),
    );
    stable.insert("model", json!(model));
    stable.insert("op", json!(BATCH_OPERATION));
    stable.insert("required_epoch", json!(identity.table_epoch));
    stable.insert("required_fingerprint", json!(identity.fingerprint));
    let encoded = serde_json::to_vec(&stable).expect("stable request-key value is serializable");
    hex_sha256(&encoded)
}

fn split_batch_pages<'a>(
    items: &'a [BatchItem],
    rows: usize,
    token_budget: usize,
) -> Vec<Vec<BatchItem>> {
    let mut pages = Vec::new();
    let mut page = Vec::new();
    let mut page_tokens = 0usize;
    for item in items {
        let item_tokens = item.text.chars().count().div_ceil(4).max(1);
        if !page.is_empty()
            && (page.len() >= rows || page_tokens.saturating_add(item_tokens) > token_budget)
        {
            pages.push(std::mem::take(&mut page));
            page_tokens = 0;
        }
        page.push(item.clone());
        page_tokens = page_tokens.saturating_add(item_tokens);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_string)
}

fn integer_field(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

fn string_array_field(value: &Value, name: &str) -> Vec<String> {
    value
        .get(name)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn content_sha256(text: &str) -> String {
    hex_sha256(text.as_bytes())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_synapse_capture_directory_is_unset_with_an_injected_lookup() {
        assert_eq!(
            capture_directory_with(|key| {
                assert_eq!(key, "AFT_SYNAPSE_CAPTURE_DIR");
                Some(std::ffi::OsString::new())
            }),
            None
        );
        assert_eq!(
            capture_directory_with(|_| Some(std::ffi::OsString::from("/captures"))),
            Some(PathBuf::from("/captures"))
        );
    }

    fn item(id: &str, text: &str) -> BatchItem {
        BatchItem {
            id: id.to_string(),
            text: text.to_string(),
            content_sha256: content_sha256(text),
        }
    }

    #[test]
    fn models_list_live_shapes_allow_optional_dims() {
        let models = parse_models_list(include_bytes!(
            "../tests/fixtures/synapse/models-list-live-raw.json"
        ))
        .unwrap();
        assert_eq!(models.len(), 7);
        let selected = models
            .iter()
            .find(|model| model.model == "gte-modernbert-base-ane-fp16")
            .unwrap();
        assert_eq!(selected.dims, None);
        assert_eq!(selected.recommended_rows, 8);
        assert_eq!(selected.recommended_token_budget, 4096);
    }

    #[test]
    fn captured_live_error_envelope_is_typed() {
        let raw = include_bytes!("../tests/fixtures/synapse/embed-query-0-live.json").to_vec();
        let error = decode_result_envelope(raw).unwrap_err();
        assert!(matches!(error, SynapseEmbeddingError::ModelNotCertified(_)));
    }

    #[test]
    fn content_hash_mismatch_is_loud_and_typed() {
        let expected = item("item:0", "hello");
        let raw = br#"{
          "fingerprint":"fp-a","table_epoch":7,
          "vectors":[{"id":"item:0","content_sha256":"wrong","vector":[1.0]}]
        }"#;
        let error = parse_embedding_page(raw, &[expected]).unwrap_err();
        assert!(matches!(
            error,
            SynapseEmbeddingError::ContentHashMismatch { .. }
        ));
        assert!(error
            .to_string()
            .contains("synapse_content_sha256_mismatch"));
    }

    #[test]
    fn fingerprint_equivalence_class_accepts_aliases() {
        let mut identity = SynapseIdentity {
            fingerprint: "fp-current".to_string(),
            table_epoch: 7,
            equivalent_to: vec!["fp-old".to_string()],
        };
        assert!(identity.accepts("fp-old"));
        assert!(identity.observe_equivalence("fp-new", &["fp-current".to_string()]));
        assert!(identity.accepts("fp-new"));
        assert!(!identity.accepts("foreign"));
        assert!(!identity.observe_equivalence("foreign", &[]));
    }

    #[test]
    fn pages_reassemble_by_chunk_and_item_id() {
        let expected = vec![item("item:0", "a"), item("item:1", "b")];
        let first = parse_embedding_page(
            include_bytes!("../tests/fixtures/synapse/embed-batch-page-1.json"),
            &expected,
        )
        .unwrap();
        let second = parse_embedding_page(
            include_bytes!("../tests/fixtures/synapse/embed-batch-page-2.json"),
            &expected,
        )
        .unwrap();
        assert_eq!(first.next_chunk_id.as_deref(), Some("chunk-2"));
        let mut vectors = first.vectors;
        vectors.extend(second.vectors);
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors["item:1"], vec![0.0, 1.0]);
    }

    #[test]
    fn request_key_is_stable_for_idempotent_retry() {
        let identity = SynapseIdentity {
            fingerprint: "fp-a".to_string(),
            table_epoch: 7,
            equivalent_to: Vec::new(),
        };
        let items = vec![item("item:0", "hello")];
        assert_eq!(
            batch_request_key("configured-model", &identity, &items),
            batch_request_key("configured-model", &identity, &items)
        );
    }
}
