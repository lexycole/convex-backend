#![feature(iterator_try_collect)]
#![feature(lazy_cell)]

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use common::{
    execution_context::ExecutionId,
    types::{
        ModuleEnvironment,
        UdfIdentifier,
    },
};
use events::usage::{
    UsageEvent,
    UsageEventLogger,
};
use parking_lot::Mutex;
use pb::usage::{
    CounterWithTag as CounterWithTagProto,
    FunctionUsageStats as FunctionUsageStatsProto,
};
use value::heap_size::WithHeapSize;

mod metrics;

/// The core usage stats aggregator that is cheaply cloneable
#[derive(Clone, Debug)]
pub struct UsageCounter {
    usage_logger: Arc<dyn UsageEventLogger>,
}

impl UsageCounter {
    pub fn new(usage_logger: Arc<dyn UsageEventLogger>) -> Self {
        Self { usage_logger }
    }
}

pub enum CallType {
    Action {
        env: ModuleEnvironment,
        duration: Duration,
        memory_in_mb: u64,
    },
    HttpAction {
        duration: Duration,
        memory_in_mb: u64,
    },
    Export,
    CachedQuery,
    UncachedQuery,
    Mutation,
    Import,
}

impl CallType {
    fn tag(&self) -> &'static str {
        match self {
            Self::Action { .. } => "action",
            Self::Export => "export",
            Self::CachedQuery => "cached_query",
            Self::UncachedQuery => "uncached_query",
            Self::Mutation => "mutation",
            Self::HttpAction { .. } => "http_action",
            Self::Import => "import",
        }
    }

    fn memory_megabytes(&self) -> u64 {
        match self {
            CallType::Action { memory_in_mb, .. } | CallType::HttpAction { memory_in_mb, .. } => {
                *memory_in_mb
            },
            _ => 0,
        }
    }

    fn duration_millis(&self) -> u64 {
        match self {
            CallType::Action { duration, .. } | CallType::HttpAction { duration, .. } => {
                u64::try_from(duration.as_millis())
                    .expect("Action was running for over 584 billion years??")
            },
            _ => 0,
        }
    }

    fn environment(&self) -> String {
        match self {
            CallType::Action { env, .. } => env,
            // All other UDF types, including HTTP actions run on the isolate
            // only.
            _ => &ModuleEnvironment::Isolate,
        }
        .to_string()
    }
}

impl UsageCounter {
    pub fn track_call(
        &self,
        udf_path: UdfIdentifier,
        execution_id: ExecutionId,
        call_type: CallType,
        stats: FunctionUsageStats,
    ) {
        let mut usage_metrics = Vec::new();

        // Because system udfs might cause usage before any data is added by the user,
        // we do not count their calls. We do count their bandwidth.
        let (should_track_calls, udf_id_type) = match &udf_path {
            UdfIdentifier::Function(path) => (!path.is_system(), "function"),
            UdfIdentifier::Http(_) => (true, "http"),
            UdfIdentifier::Cli(_) => (false, "cli"),
        };
        usage_metrics.push(UsageEvent::FunctionCall {
            id: execution_id.to_string(),
            udf_id: udf_path.to_string(),
            udf_id_type: udf_id_type.to_string(),
            tag: call_type.tag().to_string(),
            memory_megabytes: call_type.memory_megabytes(),
            duration_millis: call_type.duration_millis(),
            environment: call_type.environment(),
            is_tracked: should_track_calls,
        });

        // We always track bandwidth, even for system udfs.
        self._track_function_usage(udf_path, stats, execution_id, &mut usage_metrics);
        self.usage_logger.record(usage_metrics);
    }

    // TODO: The existence of this function is a hack due to shortcuts we have
    // done in Node.js usage tracking. It should only be used by Node.js action
    // callbacks. We should only be using track_call() and never calling this
    // this directly. Otherwise, we will have the usage reflected in the usage
    // stats for billing but not in the UDF execution log counters.
    pub fn track_function_usage(
        &self,
        udf_path: UdfIdentifier,
        execution_id: ExecutionId,
        stats: FunctionUsageStats,
    ) {
        let mut usage_metrics = Vec::new();
        self._track_function_usage(udf_path, stats, execution_id, &mut usage_metrics);
        self.usage_logger.record(usage_metrics);
    }

    pub fn _track_function_usage(
        &self,
        udf_path: UdfIdentifier,
        stats: FunctionUsageStats,
        execution_id: ExecutionId,
        usage_metrics: &mut Vec<UsageEvent>,
    ) {
        // Merge the storage stats.
        for (storage_api, function_count) in stats.storage_calls {
            usage_metrics.push(UsageEvent::FunctionStorageCalls {
                id: execution_id.to_string(),
                udf_id: udf_path.to_string(),
                call: storage_api,
                count: function_count,
            });
        }
        usage_metrics.push(UsageEvent::FunctionStorageBandwidth {
            id: execution_id.to_string(),
            udf_id: udf_path.to_string(),
            ingress: stats.storage_ingress_size,
            egress: stats.storage_egress_size,
        });
        // Merge "by table" bandwidth stats.
        for (table_name, ingress_size) in stats.database_ingress_size {
            usage_metrics.push(UsageEvent::DatabaseBandwidth {
                id: execution_id.to_string(),
                udf_id: udf_path.to_string(),
                table_name,
                ingress: ingress_size,
                egress: 0,
            });
        }
        for (table_name, egress_size) in stats.database_egress_size {
            usage_metrics.push(UsageEvent::DatabaseBandwidth {
                id: execution_id.to_string(),
                udf_id: udf_path.to_string(),
                table_name,
                ingress: 0,
                egress: egress_size,
            });
        }
        for (table_name, ingress_size) in stats.vector_ingress_size {
            usage_metrics.push(UsageEvent::VectorBandwidth {
                id: execution_id.to_string(),
                udf_id: udf_path.to_string(),
                table_name,
                ingress: ingress_size,
                egress: 0,
            });
        }
        for (table_name, egress_size) in stats.vector_egress_size {
            usage_metrics.push(UsageEvent::VectorBandwidth {
                id: execution_id.to_string(),
                udf_id: udf_path.to_string(),
                table_name,
                ingress: 0,
                egress: egress_size,
            });
        }
    }
}

// We can track storage attributed by UDF or not. This is why unlike database
// and vector search egress/ingress those methods are both on
// FunctionUsageTracker and UsageCounters directly.
pub trait StorageUsageTracker: Send + Sync {
    fn track_storage_call(&self, storage_api: &'static str) -> Box<dyn StorageCallTracker>;
}

pub trait StorageCallTracker: Send + Sync {
    fn track_storage_ingress_size(&self, ingress_size: u64);
    fn track_storage_egress_size(&self, egress_size: u64);
}

struct IndependentStorageCallTracker {
    execution_id: ExecutionId,
    usage_logger: Arc<dyn UsageEventLogger>,
}

impl IndependentStorageCallTracker {
    fn new(execution_id: ExecutionId, usage_logger: Arc<dyn UsageEventLogger>) -> Self {
        Self {
            execution_id,
            usage_logger,
        }
    }
}

impl StorageCallTracker for IndependentStorageCallTracker {
    fn track_storage_ingress_size(&self, ingress_size: u64) {
        metrics::storage::log_storage_ingress_size(ingress_size);
        self.usage_logger.record(vec![UsageEvent::StorageBandwidth {
            id: self.execution_id.to_string(),
            ingress: ingress_size,
            egress: 0,
        }]);
    }

    fn track_storage_egress_size(&self, egress_size: u64) {
        metrics::storage::log_storage_egress_size(egress_size);
        self.usage_logger.record(vec![UsageEvent::StorageBandwidth {
            id: self.execution_id.to_string(),
            ingress: 0,
            egress: egress_size,
        }]);
    }
}

impl StorageUsageTracker for UsageCounter {
    fn track_storage_call(&self, storage_api: &'static str) -> Box<dyn StorageCallTracker> {
        let execution_id = ExecutionId::new();
        metrics::storage::log_storage_call();
        self.usage_logger.record(vec![UsageEvent::StorageCall {
            id: execution_id.to_string(),
            call: storage_api.to_string(),
        }]);

        Box::new(IndependentStorageCallTracker::new(
            execution_id,
            self.usage_logger.clone(),
        ))
    }
}

/// Usage tracker used within a Transaction. Note that this structure does not
/// directly report to the backend global counters and instead only buffers the
/// counters locally. The counters get rolled into the global ones via
/// UsageCounters::track_call() at the end of each UDF. This provides a
/// consistent way to account for usage, where we only bill people for usage
/// that makes it to the UdfExecution log.
#[derive(Debug, Clone)]
pub struct FunctionUsageTracker {
    // TODO: We should ideally not use an Arc<Mutex> here. The best way to achieve
    // this is to move the logic for accounting ingress out of the Committer into
    // the Transaction. Then Transaction can solely own the counters and we can
    // remove clone(). The alternative is for the Committer to take ownership of
    // the usage tracker and then return it, but this will make it complicated if
    // we later decide to charge people for OCC bandwidth.
    state: Arc<Mutex<FunctionUsageStats>>,
}

impl FunctionUsageTracker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FunctionUsageStats::default())),
        }
    }

    /// Calculate FunctionUsageStats here
    pub fn gather_user_stats(self) -> FunctionUsageStats {
        self.state.lock().clone()
    }

    /// Adds the given usage stats to the current tracker.
    pub fn add(&self, stats: FunctionUsageStats) {
        self.state.lock().merge(stats);
    }

    // Tracks database usage from write operations (insert/update/delete) for
    // documents that are not in vector indexes. If the document has one or more
    // vectors in a vector index, call `track_vector_ingress_size` instead of
    // this method.
    //
    // You must always check to see if a document is a vector index before
    // calling this method.
    pub fn track_database_ingress_size(
        &self,
        table_name: String,
        ingress_size: u64,
        skip_logging: bool,
    ) {
        if skip_logging {
            return;
        }

        let mut state = self.state.lock();
        state
            .database_ingress_size
            .mutate_entry_or_default(table_name.clone(), |count| *count += ingress_size);
    }

    pub fn track_database_egress_size(
        &self,
        table_name: String,
        egress_size: u64,
        skip_logging: bool,
    ) {
        if skip_logging {
            return;
        }

        let mut state = self.state.lock();
        state
            .database_egress_size
            .mutate_entry_or_default(table_name.clone(), |count| *count += egress_size);
    }

    // Tracks the vector ingress surcharge and database usage for documents
    // that have one or more vectors in a vector index.
    //
    // If the document does not have a vector in a vector index, call
    // `track_database_ingress_size` instead of this method.
    //
    // Vector bandwidth is a surcharge on vector related bandwidth usage. As a
    // result it counts against both bandwidth ingress and vector ingress.
    // Ingress is a bit trickier than egress because vector ingress needs to be
    // updated whenever the mutated document is in a vector index. To be in a
    // vector index the document must both be in a table with a vector index and
    // have at least one vector that's actually used in the index.
    pub fn track_vector_ingress_size(
        &self,
        table_name: String,
        ingress_size: u64,
        skip_logging: bool,
    ) {
        if skip_logging {
            return;
        }

        // Note that vector search counts as both database and vector bandwidth
        // per the comment above.
        let mut state = self.state.lock();
        state
            .database_ingress_size
            .mutate_entry_or_default(table_name.clone(), |count| {
                *count += ingress_size;
            });
        state
            .vector_ingress_size
            .mutate_entry_or_default(table_name.clone(), |count| {
                *count += ingress_size;
            });
    }

    // Tracks bandwidth usage from vector searches
    //
    // Vector bandwidth is a surcharge on vector related bandwidth usage. As a
    // result it counts against both bandwidth egress and vector egress. It's an
    // error to increment vector egress without also incrementing database
    // egress. The reverse is not true however, it's totally fine to increment
    // general database egress without incrementing vector egress if the operation
    // is not a vector search.
    //
    // Unlike track_database_ingress_size, this method is explicitly vector related
    // because we should always know that the relevant operation is a vector
    // search. In contrast, for ingress any insert/update/delete could happen to
    // impact a vector index.
    pub fn track_vector_egress_size(
        &self,
        table_name: String,
        egress_size: u64,
        skip_logging: bool,
    ) {
        if skip_logging {
            return;
        }

        // Note that vector search counts as both database and vector bandwidth
        // per the comment above.
        let mut state = self.state.lock();
        state
            .database_egress_size
            .mutate_entry_or_default(table_name.clone(), |count| *count += egress_size);
        state
            .vector_egress_size
            .mutate_entry_or_default(table_name.clone(), |count| *count += egress_size);
    }
}

// For UDFs, we track storage at the per UDF level, no finer. So we can just
// aggregate over the entire UDF and not worry about sending usage events or
// creating unique execution ids.
impl StorageCallTracker for FunctionUsageTracker {
    fn track_storage_ingress_size(&self, ingress_size: u64) {
        let mut state = self.state.lock();
        metrics::storage::log_storage_ingress_size(ingress_size);
        state.storage_ingress_size += ingress_size;
    }

    fn track_storage_egress_size(&self, egress_size: u64) {
        let mut state = self.state.lock();
        metrics::storage::log_storage_egress_size(egress_size);
        state.storage_egress_size += egress_size;
    }
}

impl StorageUsageTracker for FunctionUsageTracker {
    fn track_storage_call(&self, storage_api: &'static str) -> Box<dyn StorageCallTracker> {
        let mut state = self.state.lock();
        metrics::storage::log_storage_call();
        state
            .storage_calls
            .mutate_entry_or_default(storage_api.to_string(), |count| *count += 1);
        Box::new(self.clone())
    }
}

type TableName = String;
type StorageAPI = String;

/// User-facing UDF stats, built
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct FunctionUsageStats {
    pub storage_calls: WithHeapSize<BTreeMap<StorageAPI, u64>>,
    pub storage_ingress_size: u64,
    pub storage_egress_size: u64,
    pub database_ingress_size: WithHeapSize<BTreeMap<TableName, u64>>,
    pub database_egress_size: WithHeapSize<BTreeMap<TableName, u64>>,
    pub vector_ingress_size: WithHeapSize<BTreeMap<TableName, u64>>,
    pub vector_egress_size: WithHeapSize<BTreeMap<TableName, u64>>,
}

impl FunctionUsageStats {
    pub fn aggregate(&self) -> AggregatedFunctionUsageStats {
        AggregatedFunctionUsageStats {
            database_read_bytes: self.database_egress_size.values().sum(),
            database_write_bytes: self.database_ingress_size.values().sum(),
            storage_read_bytes: self.storage_egress_size,
            storage_write_bytes: self.storage_ingress_size,
            vector_index_read_bytes: self.vector_egress_size.values().sum(),
            vector_index_write_bytes: self.vector_ingress_size.values().sum(),
        }
    }

    fn merge(&mut self, other: Self) {
        // Merge the storage stats.
        for (storage_api, function_count) in other.storage_calls {
            self.storage_calls
                .mutate_entry_or_default(storage_api, |count| *count += function_count);
        }
        self.storage_ingress_size += other.storage_ingress_size;
        self.storage_egress_size += other.storage_egress_size;

        // Merge "by table" bandwidth other.
        for (table_name, ingress_size) in other.database_ingress_size {
            self.database_ingress_size
                .mutate_entry_or_default(table_name.clone(), |count| *count += ingress_size);
        }
        for (table_name, egress_size) in other.database_egress_size {
            self.database_egress_size
                .mutate_entry_or_default(table_name.clone(), |count| *count += egress_size);
        }
        for (table_name, ingress_size) in other.vector_ingress_size {
            self.vector_ingress_size
                .mutate_entry_or_default(table_name.clone(), |count| *count += ingress_size);
        }
        for (table_name, egress_size) in other.vector_egress_size {
            self.vector_egress_size
                .mutate_entry_or_default(table_name.clone(), |count| *count += egress_size);
        }
    }
}

fn to_by_tag_count(counts: impl Iterator<Item = (String, u64)>) -> Vec<CounterWithTagProto> {
    counts
        .map(|(tag, count)| CounterWithTagProto {
            name: Some(tag),
            count: Some(count),
        })
        .collect()
}

fn from_by_tag_count(
    counts: Vec<CounterWithTagProto>,
) -> anyhow::Result<impl Iterator<Item = (String, u64)>> {
    let counts: Vec<_> = counts
        .into_iter()
        .map(|c| -> anyhow::Result<_> {
            let name = c.name.context("Missing `tag` field")?;
            let count = c.count.context("Missing `count` field")?;
            Ok((name, count))
        })
        .try_collect()?;
    Ok(counts.into_iter())
}

impl From<FunctionUsageStats> for FunctionUsageStatsProto {
    fn from(stats: FunctionUsageStats) -> Self {
        FunctionUsageStatsProto {
            storage_calls: to_by_tag_count(stats.storage_calls.into_iter()),
            storage_ingress_size: Some(stats.storage_ingress_size),
            storage_egress_size: Some(stats.storage_egress_size),
            database_ingress_size: to_by_tag_count(stats.database_ingress_size.into_iter()),
            database_egress_size: to_by_tag_count(stats.database_egress_size.into_iter()),
            vector_ingress_size: to_by_tag_count(stats.vector_ingress_size.into_iter()),
            vector_egress_size: to_by_tag_count(stats.vector_egress_size.into_iter()),
        }
    }
}

impl TryFrom<FunctionUsageStatsProto> for FunctionUsageStats {
    type Error = anyhow::Error;

    fn try_from(stats: FunctionUsageStatsProto) -> anyhow::Result<Self> {
        let storage_calls = from_by_tag_count(stats.storage_calls)?.collect();
        let storage_ingress_size = stats
            .storage_ingress_size
            .context("Missing `storage_ingress_size` field")?;
        let storage_egress_size = stats
            .storage_egress_size
            .context("Missing `storage_egress_size` field")?;
        let database_ingress_size = from_by_tag_count(stats.database_ingress_size)?.collect();
        let database_egress_size = from_by_tag_count(stats.database_egress_size)?.collect();
        let vector_ingress_size = from_by_tag_count(stats.vector_ingress_size)?.collect();
        let vector_egress_size = from_by_tag_count(stats.vector_egress_size)?.collect();

        Ok(FunctionUsageStats {
            storage_calls,
            storage_ingress_size,
            storage_egress_size,
            database_ingress_size,
            database_egress_size,
            vector_ingress_size,
            vector_egress_size,
        })
    }
}

/// User-facing UDF stats, that is logged in the UDF execution log
/// and might be used for debugging purposes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AggregatedFunctionUsageStats {
    pub database_read_bytes: u64,
    pub database_write_bytes: u64,
    pub storage_read_bytes: u64,
    pub storage_write_bytes: u64,
    pub vector_index_read_bytes: u64,
    pub vector_index_write_bytes: u64,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use value::testing::assert_roundtrips;

    use super::{
        FunctionUsageStats,
        FunctionUsageStatsProto,
    };

    proptest! {
        #![proptest_config(
            ProptestConfig { failure_persistence: None, ..ProptestConfig::default() }
        )]

        #[test]
        fn test_usage_stats_roundtrips(stats in any::<FunctionUsageStats>()) {
            assert_roundtrips::<FunctionUsageStats, FunctionUsageStatsProto>(stats);
        }
    }
}
