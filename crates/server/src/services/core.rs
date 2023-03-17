use anyhow::Error;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use warg_api::content::ContentSource;
use warg_crypto::hash::{DynHash, Hash, Sha256};
use warg_protocol::registry::LogLeaf;
use warg_protocol::{
    operator, package,
    registry::{LogId, MapCheckpoint, RecordId},
    ProtoEnvelope, SerdeEnvelope,
};

#[derive(Clone, Debug)]
pub struct State {
    checkpoints: Vec<Arc<SerdeEnvelope<MapCheckpoint>>>,
    checkpoint_index: HashMap<Hash<Sha256>, usize>,
    operator_info: Arc<Mutex<OperatorInfo>>,
    package_states: HashMap<LogId, Arc<Mutex<PackageInfo>>>,
}

impl State {
    pub fn new(
        init_checkpoint: SerdeEnvelope<MapCheckpoint>,
        init_record: ProtoEnvelope<operator::OperatorRecord>,
    ) -> Self {
        let checkpoint_hash: Hash<Sha256> = Hash::of(init_checkpoint.as_ref());
        let checkpoint = Arc::new(init_checkpoint);
        let record = Arc::new(init_record);

        let checkpoints = vec![checkpoint.clone()];

        let mut validator = operator::Validator::default();
        validator.validate(&record).unwrap();

        let log = vec![record.clone()];

        let mut records = HashMap::new();
        let record_info = OperatorRecordInfo {
            record: record.clone(),
            state: RecordState::Published { checkpoint },
        };
        records.insert(RecordId::operator_record::<Sha256>(&record), record_info);
        let checkpoint_indices = vec![0];

        let operator_info = OperatorInfo {
            validator,
            log,
            records,
            checkpoint_indices,
        };
        Self {
            checkpoints,
            checkpoint_index: HashMap::from([(checkpoint_hash, 0)]),
            operator_info: Arc::new(Mutex::new(operator_info)),
            package_states: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OperatorInfo {
    validator: operator::Validator,
    log: Vec<Arc<ProtoEnvelope<operator::OperatorRecord>>>,
    checkpoint_indices: Vec<usize>,
    records: HashMap<RecordId, OperatorRecordInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OperatorRecordInfo {
    record: Arc<ProtoEnvelope<operator::OperatorRecord>>,
    state: RecordState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PackageInfo {
    id: LogId,
    name: String,
    validator: package::Validator,
    log: Vec<Arc<ProtoEnvelope<package::PackageRecord>>>,
    checkpoint_indices: Vec<usize>,
    records: HashMap<RecordId, PackageRecordInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRecordInfo {
    pub record: Arc<ProtoEnvelope<package::PackageRecord>>,
    pub content_sources: Arc<Vec<ContentSource>>,
    pub state: RecordState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RecordState {
    #[default]
    Unknown,
    Processing,
    Published {
        checkpoint: Arc<SerdeEnvelope<MapCheckpoint>>,
    },
    Rejected {
        reason: String,
    },
}

pub struct CoreService {
    mailbox: mpsc::Sender<Message>,
    _handle: JoinHandle<State>,
}

#[derive(Debug)]
enum Message {
    SubmitPackageRecord {
        package_name: String,
        record: Arc<ProtoEnvelope<package::PackageRecord>>,
        content_sources: Vec<ContentSource>,
        response: oneshot::Sender<RecordState>,
    },
    GetPackageRecordStatus {
        package_id: LogId,
        record_id: RecordId,
        response: oneshot::Sender<RecordState>,
    },
    GetPackageRecordInfo {
        package_id: LogId,
        record_id: RecordId,
        response: oneshot::Sender<Option<PackageRecordInfo>>,
    },
    NewCheckpoint {
        checkpoint: Arc<SerdeEnvelope<MapCheckpoint>>,
        leaves: Vec<LogLeaf>,
    },
    FetchOperatorRecords {
        root: Hash<Sha256>,
        since: Option<RecordId>,
        response: oneshot::Sender<Result<Vec<Arc<ProtoEnvelope<operator::OperatorRecord>>>, Error>>,
    },
    FetchPackageRecords {
        root: Hash<Sha256>,
        package_id: LogId,
        since: Option<RecordId>,
        response: oneshot::Sender<Result<Vec<Arc<ProtoEnvelope<package::PackageRecord>>>, Error>>,
    },
    GetLatestCheckpoint {
        response: oneshot::Sender<Arc<SerdeEnvelope<MapCheckpoint>>>,
    },
}

impl CoreService {
    pub fn start(initial_state: State, transparency_tx: Sender<LogLeaf>) -> Self {
        let (mailbox, rx) = mpsc::channel::<Message>(4);
        let _handle =
            tokio::spawn(async move { Self::process(initial_state, rx, transparency_tx).await });

        Self { mailbox, _handle }
    }

    async fn process(
        initial_state: State,
        mut rx: Receiver<Message>,
        transparency_tx: Sender<LogLeaf>,
    ) -> State {
        let mut state = initial_state;

        while let Some(request) = rx.recv().await {
            tracing::trace!(?request, "CoreService processing request");
            match request {
                Message::SubmitPackageRecord {
                    package_name,
                    record,
                    content_sources,
                    response,
                } => {
                    let package_id = LogId::package_log::<Sha256>(&package_name);
                    let package_info = state
                        .package_states
                        .entry(package_id.clone())
                        .or_insert_with(|| {
                            Arc::new(Mutex::new(PackageInfo {
                                id: package_id,
                                name: package_name,
                                validator: Default::default(),
                                log: Default::default(),
                                checkpoint_indices: Default::default(),
                                records: Default::default(),
                            }))
                        })
                        .clone();
                    let transparency_tx = transparency_tx.clone();
                    tokio::spawn(async move {
                        new_record(
                            package_info,
                            record,
                            content_sources,
                            response,
                            transparency_tx,
                        )
                        .await
                    });
                }
                Message::GetPackageRecordStatus {
                    package_id,
                    record_id,
                    response,
                } => {
                    if let Some(package_info) = state.package_states.get(&package_id).cloned() {
                        tokio::spawn(async move {
                            let info = package_info.as_ref().lock().await;
                            if let Some(record_info) = info.records.get(&record_id) {
                                response.send(record_info.state.clone()).unwrap();
                            } else {
                                response.send(RecordState::Unknown).unwrap();
                            }
                        });
                    } else {
                        response.send(RecordState::Unknown).unwrap();
                    }
                }
                Message::GetPackageRecordInfo {
                    package_id,
                    record_id,
                    response,
                } => {
                    if let Some(package_info) = state.package_states.get(&package_id).cloned() {
                        tokio::spawn(async move {
                            let info = package_info.as_ref().lock().await;
                            if let Some(record_info) = info.records.get(&record_id) {
                                response.send(Some(record_info.clone())).unwrap();
                            } else {
                                response.send(None).unwrap();
                            }
                        });
                    } else {
                        response.send(None).unwrap();
                    }
                }
                Message::NewCheckpoint { checkpoint, leaves } => {
                    let checkpoint_index = state.checkpoints.len();
                    state.checkpoints.push(checkpoint.clone());
                    state
                        .checkpoint_index
                        .insert(Hash::of(checkpoint.as_ref().as_ref()), checkpoint_index);
                    for leaf in leaves {
                        let package_info = state.package_states.get(&leaf.log_id).unwrap().clone();
                        let checkpoint_clone = checkpoint.clone();
                        tokio::spawn(async move {
                            mark_published(
                                package_info,
                                leaf.record_id,
                                checkpoint_clone,
                                checkpoint_index,
                            )
                            .await
                        });
                    }
                }
                Message::FetchOperatorRecords {
                    root,
                    since,
                    response,
                } => {
                    if let Some(&checkpoint_index) = state.checkpoint_index.get(&root) {
                        let operator_info = state.operator_info.clone();
                        tokio::spawn(async move {
                            response
                                .send(
                                    fetch_operator_records(operator_info, since, checkpoint_index)
                                        .await,
                                )
                                .unwrap();
                        });
                    } else {
                        response
                            .send(Err(Error::msg("Checkpoint not known")))
                            .unwrap();
                    }
                }
                Message::FetchPackageRecords {
                    root,
                    package_id,
                    since,
                    response,
                } => {
                    if let Some(&checkpoint_index) = state.checkpoint_index.get(&root) {
                        if let Some(package_info) = state.package_states.get(&package_id).cloned() {
                            tokio::spawn(async move {
                                response
                                    .send(
                                        fetch_package_records(
                                            package_info,
                                            since,
                                            checkpoint_index,
                                        )
                                        .await,
                                    )
                                    .unwrap();
                            });
                        } else {
                            response.send(Err(Error::msg("Package not found"))).unwrap();
                        }
                    } else {
                        response
                            .send(Err(Error::msg("Checkpoint not known")))
                            .unwrap();
                    }
                }
                Message::GetLatestCheckpoint { response } => response
                    .send(state.checkpoints.last().unwrap().clone())
                    .unwrap(),
            }
            tracing::trace!(?state, "Processing complete");
        }

        state
    }
}

async fn new_record(
    package_info: Arc<Mutex<PackageInfo>>,
    record: Arc<ProtoEnvelope<package::PackageRecord>>,
    content_sources: Vec<ContentSource>,
    response: oneshot::Sender<RecordState>,
    transparency_tx: Sender<LogLeaf>,
) {
    let mut info = package_info.as_ref().lock().await;
    let record_id = RecordId::package_record::<Sha256>(&record);
    let snapshot = info.validator.snapshot();
    match info.validator.validate(&record) {
        Ok(contents) => {
            let provided_contents: HashSet<DynHash> = content_sources
                .iter()
                .map(|source| source.digest.clone())
                .collect();
            for needed_content in contents {
                if !provided_contents.contains(&needed_content) {
                    let state = RecordState::Rejected {
                        reason: format!("Needed content {} but not provided", needed_content),
                    };
                    response.send(state).unwrap();
                    info.validator.rollback(snapshot);
                    return;
                }
            }

            let state = RecordState::Processing;
            let record_info = PackageRecordInfo {
                record: record.clone(),
                content_sources: Arc::new(content_sources),
                state: state.clone(),
            };

            transparency_tx
                .send(LogLeaf {
                    log_id: info.id.clone(),
                    record_id: record_id.clone(),
                })
                .await
                .unwrap();

            info.log.push(record);
            info.records.insert(record_id, record_info);

            response.send(state).unwrap();
        }
        Err(error) => {
            let reason = error.to_string();
            let state = RecordState::Rejected { reason };
            let record_info = PackageRecordInfo {
                record,
                content_sources: Arc::new(content_sources),
                state: state.clone(),
            };
            info.records.insert(record_id, record_info);

            response.send(state).unwrap();
        }
    };
}

async fn mark_published(
    package_info: Arc<Mutex<PackageInfo>>,
    record_id: RecordId,
    checkpoint: Arc<SerdeEnvelope<MapCheckpoint>>,
    checkpoint_index: usize,
) {
    let mut info = package_info.as_ref().lock().await;

    info.records.get_mut(&record_id).unwrap().state = RecordState::Published { checkpoint };
    // Requires publishes to be marked in order for correctness
    info.checkpoint_indices.push(checkpoint_index);
}

async fn fetch_operator_records(
    operator_info: Arc<Mutex<OperatorInfo>>,
    since: Option<RecordId>,
    checkpoint_index: usize,
) -> Result<Vec<Arc<ProtoEnvelope<operator::OperatorRecord>>>, Error> {
    let info = operator_info.as_ref().lock().await;

    let start = match since {
        Some(hash) => get_operator_record_index(&info.log, hash)? + 1,
        None => 0,
    };
    let end = get_records_before_checkpoint(&info.checkpoint_indices, checkpoint_index);
    let result = info.log[start..end].to_vec();
    Ok(result)
}

async fn fetch_package_records(
    package_info: Arc<Mutex<PackageInfo>>,
    since: Option<RecordId>,
    checkpoint_index: usize,
) -> Result<Vec<Arc<ProtoEnvelope<package::PackageRecord>>>, Error> {
    let info = package_info.as_ref().lock().await;

    let start = match since {
        Some(hash) => get_package_record_index(&info.log, hash)? + 1,
        None => 0,
    };
    let end = get_records_before_checkpoint(&info.checkpoint_indices, checkpoint_index);
    let result = info.log[start..end].to_vec();
    Ok(result)
}

fn get_package_record_index(
    log: &[Arc<ProtoEnvelope<package::PackageRecord>>],
    hash: RecordId,
) -> Result<usize, Error> {
    log.iter()
        .map(|env| RecordId::package_record::<Sha256>(env.as_ref()))
        .position(|found| found == hash)
        .ok_or_else(|| Error::msg("Hash value not found"))
}

fn get_operator_record_index(
    log: &[Arc<ProtoEnvelope<operator::OperatorRecord>>],
    hash: RecordId,
) -> Result<usize, Error> {
    log.iter()
        .map(|env| RecordId::operator_record::<Sha256>(env.as_ref()))
        .position(|found| found == hash)
        .ok_or_else(|| Error::msg("Hash value not found"))
}

fn get_records_before_checkpoint(indices: &[usize], checkpoint_index: usize) -> usize {
    indices
        .iter()
        .filter(|index| **index <= checkpoint_index)
        .count()
}

impl CoreService {
    pub async fn submit_package_record(
        &self,
        package_name: String,
        record: Arc<ProtoEnvelope<package::PackageRecord>>,
        content_sources: Vec<ContentSource>,
    ) -> RecordState {
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::SubmitPackageRecord {
                package_name,
                record,
                content_sources,
                response: tx,
            })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    pub async fn get_package_record_status(
        &self,
        package_id: LogId,
        record_id: RecordId,
    ) -> RecordState {
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::GetPackageRecordStatus {
                package_id,
                record_id,
                response: tx,
            })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    pub async fn get_package_record_info(
        &self,
        package_id: LogId,
        record_id: RecordId,
    ) -> Option<PackageRecordInfo> {
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::GetPackageRecordInfo {
                package_id,
                record_id,
                response: tx,
            })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    pub async fn new_checkpoint(
        &self,
        checkpoint: SerdeEnvelope<MapCheckpoint>,
        leaves: Vec<LogLeaf>,
    ) {
        self.mailbox
            .send(Message::NewCheckpoint {
                checkpoint: Arc::new(checkpoint),
                leaves,
            })
            .await
            .unwrap();
    }

    pub async fn fetch_operator_records(
        &self,
        root: DynHash,
        since: Option<RecordId>,
    ) -> Result<Vec<Arc<ProtoEnvelope<operator::OperatorRecord>>>, Error> {
        let root = root.try_into()?;
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::FetchOperatorRecords {
                root,
                since,
                response: tx,
            })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    pub async fn fetch_package_records(
        &self,
        root: DynHash,
        package_name: String,
        since: Option<RecordId>,
    ) -> Result<Vec<Arc<ProtoEnvelope<package::PackageRecord>>>, Error> {
        let root = root.try_into()?;
        let package_id = LogId::package_log::<Sha256>(&package_name);
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::FetchPackageRecords {
                root,
                package_id,
                since,
                response: tx,
            })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    pub async fn get_latest_checkpoint(&self) -> Arc<SerdeEnvelope<MapCheckpoint>> {
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send(Message::GetLatestCheckpoint { response: tx })
            .await
            .unwrap();

        rx.await.unwrap()
    }
}