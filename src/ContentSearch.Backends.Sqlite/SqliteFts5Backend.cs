using Microsoft.Data.Sqlite;
using PersonalRag.ContentSearch.Core;

namespace PersonalRag.ContentSearch.Backends.Sqlite;

public sealed class SqliteFts5Backend : IContentSearchBackend
{
    private readonly ContentBackendDiagnostics _diagnostics = new();
    private ContentCorpus? _corpus;
    private SqliteConnection? _connection;
    private string? _databasePath;
    private long _nextBlockId = 1;
    private long _nextRowId = 1;

    public string BackendId => "sqlite-fts5";

    public async Task BuildAsync(ContentCorpus corpus, CancellationToken cancellationToken)
    {
        if (_connection is not null)
        {
            await _connection.DisposeAsync().ConfigureAwait(false);
            _connection = null;
        }

        string directory = Path.Combine(corpus.WorkDirectory, BackendId);
        if (Directory.Exists(directory)) Directory.Delete(directory, recursive: true);
        Directory.CreateDirectory(directory);

        _corpus = corpus;
        _databasePath = Path.Combine(directory, "content.db");
        _connection = new SqliteConnection($"Data Source={_databasePath};Mode=ReadWriteCreate;Cache=Shared");
        await _connection.OpenAsync(cancellationToken).ConfigureAwait(false);

        await ExecuteNonQueryAsync(
            """
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            CREATE TABLE block_meta(
                rowid INTEGER PRIMARY KEY,
                block_id INTEGER NOT NULL,
                file_key TEXT NOT NULL,
                path TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                decoded_start INTEGER NOT NULL,
                base_line INTEGER NOT NULL,
                active INTEGER NOT NULL
            );
            CREATE INDEX ix_block_meta_file_key ON block_meta(file_key);
            CREATE VIRTUAL TABLE content_fts USING fts5(
                body,
                tokenize='trigram'
            );
            """,
            cancellationToken).ConfigureAwait(false);

        using SqliteTransaction transaction = _connection.BeginTransaction();
        foreach (ContentDocument document in corpus.Documents)
        {
            cancellationToken.ThrowIfCancellationRequested();
            await InsertDocumentAsync(document, transaction, cancellationToken).ConfigureAwait(false);
        }
        transaction.Commit();

        await ExecuteNonQueryAsync("PRAGMA wal_checkpoint(TRUNCATE);", cancellationToken).ConfigureAwait(false);
        RefreshDiagnostics();
    }

    public async Task<IReadOnlyList<ContentMatch>> SearchAsync(ContentQuery query, CancellationToken cancellationToken)
    {
        SqliteConnection connection = _connection ?? throw new InvalidOperationException("Backend is not built.");

        bool fallback =
            query.Mode != ContentQueryMode.Substring ||
            ContentSemantics.RuneCount(query.Text) < 3 ||
            !ContentSemantics.IsAscii(query.Text);

        string sql = fallback
            ? """
              SELECT m.rowid,m.block_id,m.file_key,m.path,m.ordinal,m.decoded_start,m.base_line
              FROM block_meta m
              WHERE m.active=1
              ORDER BY m.block_id;
              """
            : """
              SELECT m.rowid,m.block_id,m.file_key,m.path,m.ordinal,m.decoded_start,m.base_line
              FROM block_meta m
              JOIN content_fts f ON f.rowid=m.rowid
              WHERE m.active=1 AND content_fts MATCH $query
              ORDER BY m.block_id;
              """;

        using SqliteCommand command = connection.CreateCommand();
        command.CommandText = sql;
        if (!fallback)
            command.Parameters.AddWithValue("$query", ContentSemantics.EscapeFtsPhrase(query.Text));

        var matches = new List<ContentMatch>();
        long candidates = 0;
        long bytes = 0;

        await using SqliteDataReader reader = await command.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
        while (await reader.ReadAsync(cancellationToken).ConfigureAwait(false))
        {
            cancellationToken.ThrowIfCancellationRequested();
            candidates++;

            long rowId = reader.GetInt64(0);
            long blockId = reader.GetInt64(1);
            var fileKey = new ContentFileKey(reader.GetString(2));
            string path = reader.GetString(3);
            int ordinal = reader.GetInt32(4);
            long decodedStart = reader.GetInt64(5);
            int baseLine = reader.GetInt32(6);
            // Do not select every fallback body into one SQLite result set.  The
            // fallback path is intentionally broad, so materializing all bodies
            // at once can exhaust process memory before the cancellation token is
            // observed.  Fetch one body at a time and verify it immediately.
            string body = await ReadBodyAsync(connection, rowId, cancellationToken).ConfigureAwait(false);
            int utf8Bytes = System.Text.Encoding.UTF8.GetByteCount(body);
            bytes += utf8Bytes;

            var block = new StoredContentBlock(
                blockId,
                fileKey,
                path,
                ordinal,
                decodedStart,
                baseLine,
                0,
                utf8Bytes);

            matches.AddRange(ContentExactVerifier.Verify(block, body, query, BackendId, fallback));
        }

        _diagnostics.SearchCount++;
        _diagnostics.BytesRead += bytes;
        _diagnostics.CandidateBlocks += candidates;
        _diagnostics.VerifiedBlocks += candidates;
        if (fallback) _diagnostics.ScanFallbackCount++;

        return matches
            .GroupBy(m => (m.FileKey, m.DecodedCharOffset, m.MatchLength))
            .Select(g => g.First())
            .OrderBy(m => m.ExactPath, StringComparer.Ordinal)
            .ThenBy(m => m.DecodedCharOffset)
            .ToArray();
    }

    private static async Task<string> ReadBodyAsync(
        SqliteConnection connection,
        long rowId,
        CancellationToken cancellationToken)
    {
        using SqliteCommand command = connection.CreateCommand();
        command.CommandText = "SELECT body FROM content_fts WHERE rowid=$rowid;";
        command.Parameters.AddWithValue("$rowid", rowId);
        object? value = await command.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false);
        return value as string ?? throw new InvalidDataException($"FTS body row {rowId} is missing.");
    }

    public async Task ApplyChangesAsync(IReadOnlyList<ContentChange> changes, CancellationToken cancellationToken)
    {
        SqliteConnection connection = _connection ?? throw new InvalidOperationException("Backend is not built.");

        using SqliteTransaction transaction = connection.BeginTransaction();
        foreach (ContentChange change in changes)
        {
            cancellationToken.ThrowIfCancellationRequested();
            await DeleteFileAsync(change.FileKey, transaction, cancellationToken).ConfigureAwait(false);
            if (change.Kind != ContentChangeKind.Removed && change.Document is not null)
                await InsertDocumentAsync(change.Document, transaction, cancellationToken).ConfigureAwait(false);
        }
        transaction.Commit();

        _diagnostics.UpdateCount += changes.Count;
        RefreshDiagnostics();
    }

    public ContentBackendDiagnostics GetDiagnostics()
    {
        RefreshDiagnostics();
        return _diagnostics.Snapshot();
    }

    public async ValueTask DisposeAsync()
    {
        if (_connection is not null)
        {
            try
            {
                using SqliteCommand command = _connection.CreateCommand();
                command.CommandText = "PRAGMA wal_checkpoint(TRUNCATE);";
                await command.ExecuteNonQueryAsync().ConfigureAwait(false);
            }
            catch
            {
                // Best-effort checkpoint during disposal.
            }

            await _connection.DisposeAsync().ConfigureAwait(false);
            _connection = null;
        }
    }

    private async Task InsertDocumentAsync(ContentDocument document, SqliteTransaction transaction, CancellationToken cancellationToken)
    {
        ContentCorpus corpus = _corpus ?? throw new InvalidOperationException("Backend is not built.");
        SqliteConnection connection = _connection ?? throw new InvalidOperationException("Backend is not built.");

        await foreach (ExtractedBlock extracted in TextExtraction.EnumerateBlocksAsync(
            document, corpus.BlockSizeChars, corpus.OverlapChars, cancellationToken).ConfigureAwait(false))
        {
            long blockId = _nextBlockId++;
            long rowId = _nextRowId++;

            using (SqliteCommand meta = connection.CreateCommand())
            {
                meta.Transaction = transaction;
                meta.CommandText =
                    """
                    INSERT INTO block_meta(
                        rowid,block_id,file_key,path,ordinal,decoded_start,base_line,active)
                    VALUES($rowid,$block,$key,$path,$ordinal,$start,$line,1);
                    """;
                meta.Parameters.AddWithValue("$rowid", rowId);
                meta.Parameters.AddWithValue("$block", blockId);
                meta.Parameters.AddWithValue("$key", document.FileKey.Value);
                meta.Parameters.AddWithValue("$path", document.ExactPath);
                meta.Parameters.AddWithValue("$ordinal", extracted.BlockOrdinal);
                meta.Parameters.AddWithValue("$start", extracted.DecodedCharStart);
                meta.Parameters.AddWithValue("$line", extracted.BaseLine);
                await meta.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
            }

            using (SqliteCommand fts = connection.CreateCommand())
            {
                fts.Transaction = transaction;
                fts.CommandText = "INSERT INTO content_fts(rowid,body) VALUES($rowid,$body);";
                fts.Parameters.AddWithValue("$rowid", rowId);
                fts.Parameters.AddWithValue("$body", extracted.Text);
                await fts.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
            }

            _diagnostics.IndexedBlocks++;
            _diagnostics.ActiveBlocks++;
        }
    }

    private async Task DeleteFileAsync(ContentFileKey fileKey, SqliteTransaction transaction, CancellationToken cancellationToken)
    {
        SqliteConnection connection = _connection ?? throw new InvalidOperationException("Backend is not built.");
        var rowIds = new List<long>();

        using (SqliteCommand select = connection.CreateCommand())
        {
            select.Transaction = transaction;
            select.CommandText = "SELECT rowid FROM block_meta WHERE file_key=$key AND active=1;";
            select.Parameters.AddWithValue("$key", fileKey.Value);
            await using SqliteDataReader reader = await select.ExecuteReaderAsync(cancellationToken).ConfigureAwait(false);
            while (await reader.ReadAsync(cancellationToken).ConfigureAwait(false))
                rowIds.Add(reader.GetInt64(0));
        }

        foreach (long rowId in rowIds)
        {
            using SqliteCommand deleteFts = connection.CreateCommand();
            deleteFts.Transaction = transaction;
            deleteFts.CommandText = "DELETE FROM content_fts WHERE rowid=$rowid;";
            deleteFts.Parameters.AddWithValue("$rowid", rowId);
            await deleteFts.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
        }

        using (SqliteCommand deleteMeta = connection.CreateCommand())
        {
            deleteMeta.Transaction = transaction;
            deleteMeta.CommandText = "DELETE FROM block_meta WHERE file_key=$key;";
            deleteMeta.Parameters.AddWithValue("$key", fileKey.Value);
            await deleteMeta.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
        }

        _diagnostics.ActiveBlocks = Math.Max(0, _diagnostics.ActiveBlocks - rowIds.Count);
    }

    private async Task ExecuteNonQueryAsync(string sql, CancellationToken cancellationToken)
    {
        SqliteConnection connection = _connection ?? throw new InvalidOperationException("Backend is not built.");
        using SqliteCommand command = connection.CreateCommand();
        command.CommandText = sql;
        await command.ExecuteNonQueryAsync(cancellationToken).ConfigureAwait(false);
    }

    private void RefreshDiagnostics()
    {
        if (_databasePath is null) return;
        string directory = Path.GetDirectoryName(_databasePath)!;
        _diagnostics.PersistentBytes = Directory.Exists(directory)
            ? Directory.EnumerateFiles(directory, "content.db*", SearchOption.TopDirectoryOnly)
                .Sum(path => new FileInfo(path).Length)
            : 0;
    }
}
