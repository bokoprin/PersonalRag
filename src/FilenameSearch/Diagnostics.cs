namespace PersonalRag.FilenameSearch;

/// <summary>
/// Internal runtime counters used by the formal filename acceptance runner. The public
/// catalog contract intentionally remains unchanged.
/// </summary>
internal sealed record FilenameCatalogDiagnostics(
    int PendingEvents,
    int MaxPendingEvents,
    long QueueSaturationCount,
    long ReconcileCount,
    long CompactionCount,
    long FullBaseRewriteCount,
    long DeltaChangeCount,
    long DeltaBytes,
    long PersistenceWriteBytes,
    long Generation,
    long BaseGeneration);
