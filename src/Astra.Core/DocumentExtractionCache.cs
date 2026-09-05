namespace Astra.Core;

internal sealed class DocumentExtractionCache(long maxBytes = 384L * 1024 * 1024)
{
    private sealed record Entry(long Size, long ModifiedTicks, TextUnit[] Units, long Bytes);
    private readonly object sync = new();
    private readonly Dictionary<string, (Entry Entry, LinkedListNode<string> Node)> entries = new(StringComparer.OrdinalIgnoreCase);
    private readonly LinkedList<string> lru = new();
    private long bytes;

    public bool TryGet(string path, FileInfo info, out TextUnit[] units)
    {
        lock (sync)
        {
            if (entries.TryGetValue(path, out var found))
            {
                if (found.Entry.Size == info.Length && found.Entry.ModifiedTicks == info.LastWriteTimeUtc.Ticks)
                {
                    lru.Remove(found.Node); lru.AddFirst(found.Node); units = found.Entry.Units; return true;
                }
                Remove(path, found);
            }
        }
        units = [];
        return false;
    }

    public void Put(string path, FileInfo info, TextUnit[] units)
    {
        long estimated = units.Sum(u => 64L + ((long)u.Location.Length + u.Text.Length) * sizeof(char));
        if (estimated <= 0 || estimated > maxBytes / 2) return;
        lock (sync)
        {
            if (entries.TryGetValue(path, out var old)) Remove(path, old);
            var node = lru.AddFirst(path); var entry = new Entry(info.Length, info.LastWriteTimeUtc.Ticks, units, estimated);
            entries[path] = (entry, node); bytes += estimated;
            while (bytes > maxBytes && lru.Last is { } last)
            {
                string key = last.Value;
                if (entries.TryGetValue(key, out var victim)) Remove(key, victim); else lru.RemoveLast();
            }
        }
    }

    private void Remove(string path, (Entry Entry, LinkedListNode<string> Node) value)
    {
        entries.Remove(path); lru.Remove(value.Node); bytes -= value.Entry.Bytes;
    }
}
