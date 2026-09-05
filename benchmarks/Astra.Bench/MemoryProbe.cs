using System.Diagnostics;

internal sealed class MemoryProbe : IAsyncDisposable
{
    private readonly CancellationTokenSource stop = new();
    private readonly Task sampler;
    public long PeakPrivateBytes { get; private set; }

    public MemoryProbe(int intervalMs = 20)
    {
        Sample();
        sampler = Task.Run(async () =>
        {
            try
            {
                while (!stop.IsCancellationRequested)
                {
                    Sample();
                    await Task.Delay(intervalMs, stop.Token);
                }
            }
            catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
            finally { Sample(); }
        });
    }

    private void Sample()
    {
        using var process = Process.GetCurrentProcess();
        process.Refresh();
        PeakPrivateBytes = Math.Max(PeakPrivateBytes, process.PrivateMemorySize64);
    }

    public static long CurrentPrivateBytes()
    {
        using var process = Process.GetCurrentProcess();
        process.Refresh();
        return process.PrivateMemorySize64;
    }

    public static void FullGc()
    {
        GC.Collect(GC.MaxGeneration, GCCollectionMode.Forced, true, true);
        GC.WaitForPendingFinalizers();
        GC.Collect(GC.MaxGeneration, GCCollectionMode.Forced, true, true);
    }

    public async ValueTask DisposeAsync()
    {
        if (stop.IsCancellationRequested) return;
        stop.Cancel();
        try { await sampler; }
        finally { stop.Dispose(); }
    }
}
