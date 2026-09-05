# Content engine boundary

The filename phase deliberately stops at deterministic filename and full-path search. A future
content engine may be added beside `src/FilenameSearch` and called by a query planner, but it must
not change the `FilenameSearchEngine`, `FileSystemCatalog`, or the Frozen GUI filename contract.

The current product API exposes:

```text
SearchRequest(Query, Scope, CaseSensitive, Limit, RequestId)
FilenameRecord(FileId, ParentId, Name, FullPath, SizeBytes, ModifiedUtc, Flags)
FilenameSearchResult(Records, ElapsedMs, Candidates, UsedScan, RequestId)
```

No document bytes are read by this API. In particular, the filename product does not implement
DOCX/XLSX/PPTX/PDF extraction, regex content search, OCR, LLM, embeddings, or a vector database.
A content engine can add a separate content request/result and compose its results at a higher
layer without rewriting filesystem discovery, stable metadata, persistence recovery, or the
filename index.
