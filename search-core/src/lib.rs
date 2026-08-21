#![forbid(unsafe_op_in_unsafe_fn)]

mod builder;
mod format;
mod generation;
mod index;
mod integration;
mod mapped_file;
mod types;
mod vnext_fixed;
mod vnext_generation;
mod vnext_generation_store;
mod vnext_q3;
mod vnext_query;
mod vnext_query_simd;
mod vnext_segment;

pub use builder::{
    BuildMode, BuildOptions, BuildReport, BuildTuning, DiskPathBuildConfig, DiskPathBuildProgress,
    DiskPathBuildReport, DiskPathBuildTimings, DiskPathInput, build_disk_corpus,
    build_disk_corpus_parallel, build_disk_index_pipelined, build_disk_index_pipelined_benchmark,
    build_disk_path_inputs_index_pipelined, build_disk_path_inputs_index_unified,
    build_disk_path_inputs_index_unified_observed, build_disk_path_inputs_index_unified_retained,
    build_disk_paths_index_pipelined, build_index, build_index_benchmark, build_index_unified,
    build_index_unified_benchmark, detected_available_memory_bytes, recommend_build_tuning,
    recommend_system_build_tuning,
};
pub use format::{BuilderKind, Q3Encoding, SearchError};
pub use generation::{
    CompactionAutoPolicy, CompactionDecision, CompactionMetrics, CompactionReasons,
    GenerationReport, LogicalDocument, LogicalDocumentIdentity, MergedIndex, MergedSearchSession,
    compact_generation, compact_generation_unified, initialize_generation,
    initialize_generation_from_built_index, publish_incremental_update,
    publish_incremental_update_unified, verify_generation,
};
pub use index::{
    AccelerationProfile, ContentPlanMode, ContentQueryPlan, ContentSearchDiagnostics,
    LazyPersistentIndex, PersistentIndex, PooledLazyPersistentIndex, Pos2BuildReport,
    Pos3BuildReport, Pos3Policy, Pos23BuildReport, PosCodec, PosSidecarBuildReport,
    Positional2Index, Positional3Index, PositionalIndex, Q2SidecarBuildReport, SearchSession,
    SegmentReader, build_positional_sidecars, build_positional2_sidecars,
    build_positional3_sidecars, build_positional23_sidecars, build_q2_sidecars, verify_index,
    verify_positional_sidecars, verify_positional2_sidecars, verify_positional3_sidecars,
};
pub use integration::{
    CatalogEntry, CatalogSnapshot, ChangeBatch, ChangeKind, DocumentChange, IncrementalPolicy,
    PlannedUpsert, UpdatePlan, apply_update_plan, plan_incremental_update,
};
pub use types::{DocumentInput, Generation, LogicalDocId};
pub use vnext_generation::{
    VNextGenerationIndex, VNextGenerationLayerKind, VNextGenerationLayerSpec,
};
pub use vnext_generation_store::{
    VNextDurableCompactionReport, VNextDurableGcReport, VNextDurableGenerationReport,
    compact_vnext_generation_store, gc_vnext_generation_store, initialize_vnext_generation_store,
    initialize_vnext_generation_store_streaming, open_vnext_published_generation,
    publish_vnext_incremental_generation, verify_vnext_generation_store,
};
pub use vnext_q3::{VNextQ3Posting, VNextQ3PostingEncoding, VNextQ3PostingIter};
pub use vnext_query::{VNextContentPlanMode, VNextContentSearchDiagnostics};
pub use vnext_segment::{
    VNextBlock, VNextDocumentInput, VNextSegmentReader, VNextWriteReport, write_vnext_segment,
    write_vnext_segment_with_block_size,
};

pub fn fold_ascii(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}
