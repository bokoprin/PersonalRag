using System.Diagnostics;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text.Json;
using Astra.Core;

internal static class IdleBench
{
    public static async Task Run(string store, string result, int seconds)
    {
        await using var runtime = new IndexRuntime(store, IndexStore.Load(store));
        await ChurnBench.Until(() => runtime.IsSettled, TimeSpan.FromMinutes(5), "ready and persisted");
        using var process = Process.GetCurrentProcess();
        if (!GetProcessIoCounters(process.Handle, out var ioBefore))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessIoCounters(before) failed");
        var cpuBefore = process.TotalProcessorTime;
        long peak = process.PrivateMemorySize64;
        var watch = Stopwatch.StartNew();
        while (watch.Elapsed.TotalSeconds < seconds)
        {
            await Task.Delay(500); process.Refresh(); peak = Math.Max(peak, process.PrivateMemorySize64);
        }
        if (!GetProcessIoCounters(process.Handle, out var ioAfter))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessIoCounters(after) failed");
        double cpu = (process.TotalProcessorTime - cpuBefore).TotalMilliseconds / watch.Elapsed.TotalMilliseconds / Environment.ProcessorCount * 100;
        ulong readBytes = ioAfter.ReadTransferCount - ioBefore.ReadTransferCount, writeBytes = ioAfter.WriteTransferCount - ioBefore.WriteTransferCount;
        File.WriteAllText(result, JsonSerializer.Serialize(new { complete = seconds >= 600, formalDuration = seconds >= 600, pass = seconds >= 600 && cpu <= 1 && peak <= 2L * 1073741824 && readBytes == 0 && writeBytes == 0,
            elapsedSeconds = watch.Elapsed.TotalSeconds, averageCpuPercent = cpu, peakPrivateBytes = peak, readBytes, writeBytes, utc = DateTime.UtcNow }, new JsonSerializerOptions { WriteIndented = true }));
        Console.WriteLine($"IDLE {watch.Elapsed.TotalSeconds:F1}s / CPU {cpu:F4}% / private {peak} / read {readBytes} write {writeBytes}");
    }
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetProcessIoCounters(IntPtr process, out IoCounters counters);
    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters
    {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount, ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }
}
