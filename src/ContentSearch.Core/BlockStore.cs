using Microsoft.Win32.SafeHandles;
using System.Text;

namespace PersonalRag.ContentSearch.Core;

public sealed class ContentBlockStore : IAsyncDisposable
{
    private readonly FileStream _stream;
    private readonly SemaphoreSlim _appendLock = new(1, 1);
    private long _nextBlockId = 1;

    public ContentBlockStore(string directory)
    {
        Directory.CreateDirectory(directory);
        DataPath = Path.Combine(directory, "blocks.bin");
        _stream = new FileStream(
            DataPath,
            FileMode.Create,
            FileAccess.ReadWrite,
            FileShare.Read,
            bufferSize: 64 * 1024,
            FileOptions.Asynchronous | FileOptions.RandomAccess);
    }

    public string DataPath { get; }

    public long PersistentBytes => _stream.Length;

    public async Task<IReadOnlyList<StoredContentBlock>> AppendDocumentAsync(
        ContentDocument document,
        int blockSizeChars,
        int overlapChars,
        CancellationToken cancellationToken)
    {
        var blocks = new List<StoredContentBlock>();

        await foreach (ExtractedBlock extracted in TextExtraction.EnumerateBlocksAsync(
            document,
            blockSizeChars,
            overlapChars,
            cancellationToken).ConfigureAwait(false))
        {
            cancellationToken.ThrowIfCancellationRequested();
            byte[] bytes = Encoding.UTF8.GetBytes(extracted.Text);

            await _appendLock.WaitAsync(cancellationToken).ConfigureAwait(false);
            long offset;
            long id;
            try
            {
                offset = _stream.Length;
                _stream.Position = offset;
                await _stream.WriteAsync(bytes.AsMemory(), cancellationToken).ConfigureAwait(false);
                id = _nextBlockId++;
            }
            finally
            {
                _appendLock.Release();
            }

            blocks.Add(new StoredContentBlock(
                id,
                extracted.FileKey,
                extracted.ExactPath,
                extracted.BlockOrdinal,
                extracted.DecodedCharStart,
                extracted.BaseLine,
                offset,
                bytes.Length));
        }

        await _stream.FlushAsync(cancellationToken).ConfigureAwait(false);
        return blocks;
    }

    public async Task<string> ReadTextAsync(
        StoredContentBlock block,
        CancellationToken cancellationToken)
    {
        byte[] bytes = new byte[block.StoreLength];
        int total = 0;
        SafeFileHandle handle = _stream.SafeFileHandle;

        while (total < bytes.Length)
        {
            int read = await RandomAccess.ReadAsync(
                handle,
                bytes.AsMemory(total),
                block.StoreOffset + total,
                cancellationToken).ConfigureAwait(false);
            if (read == 0)
                throw new EndOfStreamException($"Block {block.BlockId} is truncated.");
            total += read;
        }

        return Encoding.UTF8.GetString(bytes);
    }

    public async ValueTask DisposeAsync()
    {
        await _stream.FlushAsync().ConfigureAwait(false);
        await _stream.DisposeAsync().ConfigureAwait(false);
        _appendLock.Dispose();
    }
}
