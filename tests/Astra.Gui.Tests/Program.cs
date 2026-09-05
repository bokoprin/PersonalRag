using System.Diagnostics;
using System.IO;
using System.Security.Cryptography;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Astra.Core;
using Astra.Gui;

namespace Astra.Gui.Tests;

public static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        var processWatch = Stopwatch.StartNew();
        var application = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        int exit = 0;
        application.Startup += async (_, _) =>
        {
            try
            {
                if (args.Length >= 3 && args[0].Equals("formal-startup", StringComparison.OrdinalIgnoreCase))
                    await RunFormalStartup(application, args[1], args[2], args.Length > 3 && args[3].Equals("filename-only", StringComparison.OrdinalIgnoreCase), processWatch);
                else
                    await RunFunctional(application, args.Length > 0 ? args[0] : "artifacts/gui-tests");
            }
            catch (Exception ex)
            {
                exit = 1;
                Console.Error.WriteLine(ex);
                if (args.Length >= 3 && args[0].Equals("formal-startup", StringComparison.OrdinalIgnoreCase))
                {
                    string result = Path.GetFullPath(args[2]);
                    Directory.CreateDirectory(Path.GetDirectoryName(result)!);
                    File.WriteAllText(result, JsonSerializer.Serialize(new { pass = false, error = ex.ToString(), utc = DateTime.UtcNow }, JsonOptions));
                }
            }
            finally { application.Shutdown(exit); }
        };
        application.Run();
        return exit;
    }

    private static async Task RunFormalStartup(Application application, string storeArg, string resultArg, bool filenameOnly, Stopwatch processWatch)
    {
        string store = Path.GetFullPath(storeArg), result = Path.GetFullPath(resultArg);
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        if (!File.Exists(Path.Combine(store, "snapshot.astra"))) throw new FileNotFoundException("Formal startup snapshot is missing", Path.Combine(store, "snapshot.astra"));

        MainWindow? window = null;
        try
        {
            window = new MainWindow(store) { Left = -10000, Top = -10000, ShowInTaskbar = false };
            window.Show();
            var list = (ListView)window.FindName("Results");
            var summary = (TextBlock)window.FindName("Summary");
            var content = (TextBox)window.FindName("ContentQuery");

            await Until(() => list.Items.Count > 0 && summary.Text.Contains("件表示", StringComparison.Ordinal), "formal filename first batch", TimeSpan.FromSeconds(15));
            double filenameReadyMs = processWatch.Elapsed.TotalMilliseconds;
            using var process = Process.GetCurrentProcess();
            process.Refresh();
            long filenamePrivateBytes = process.PrivateMemorySize64;
            double? contentReadyMs = null;
            long? contentPrivateBytes = null;
            int filenameRows = list.Items.Count;

            if (!filenameOnly)
            {
                content.Text = "PersonalRag";
                await Until(() => list.Items.Count > 0 && summary.Text.Contains("件表示", StringComparison.Ordinal), "formal content first batch", TimeSpan.FromSeconds(15));
                contentReadyMs = processWatch.Elapsed.TotalMilliseconds;
                process.Refresh();
                contentPrivateBytes = process.PrivateMemorySize64;
            }

            process.Refresh();
            string coreBinary = typeof(SearchEngine).Assembly.Location;
            File.WriteAllText(result, JsonSerializer.Serialize(new
            {
                pass = true,
                filenameOnly,
                filenameReadyMs,
                filenamePrivateBytes,
                contentReadyMs,
                contentPrivateBytes,
                filenameRows,
                finalRows = list.Items.Count,
                privateBytes = process.PrivateMemorySize64,
                coreBinarySha256 = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(coreBinary))),
                utc = DateTime.UtcNow
            }, JsonOptions));
            Console.WriteLine($"FORMAL GUI STARTUP filename={filenameReadyMs:F2}ms" + (contentReadyMs.HasValue ? $" content={contentReadyMs.Value:F2}ms" : ""));
        }
        finally { if (window != null) await CloseWindow(window); }
    }

    private static async Task RunFunctional(Application application, string reportArg)
    {
        string work = Path.Combine(Path.GetTempPath(), "astra-gui-tests-" + Guid.NewGuid().ToString("N"));
        string report = Path.GetFullPath(reportArg);
        Directory.CreateDirectory(report);
        MainWindow? window = null;
        var results = new List<string>();
        void Check(bool condition, string name) { if (!condition) throw new Exception("FAIL: " + name); results.Add(name); }
        try
        {
            string root = Path.Combine(work, "source"), store = Path.Combine(work, "index");
            Directory.CreateDirectory(root);
            File.WriteAllText(Path.Combine(root, "many-hits.txt"), string.Concat(Enumerable.Repeat("needle needle\n", 25000)));
            File.WriteAllText(Path.Combine(root, "日本語.txt"), "日本語の検索テスト\n");
            IndexStore.Save(store, new IndexBuilder().Build(root));
            var started = Stopwatch.StartNew();
            window = new MainWindow(store) { Left = -10000, Top = -10000, ShowInTaskbar = false };
            window.Show();
            var content = (TextBox)window.FindName("ContentQuery");
            var file = (TextBox)window.FindName("FileQuery");
            var list = (ListView)window.FindName("Results");
            var hits = (ListBox)window.FindName("HitList");
            var summary = (TextBlock)window.FindName("Summary");
            var title = (TextBlock)window.FindName("PreviewTitle");
            var mode = (ComboBox)window.FindName("Mode");
            await Until(() => list.Items.Count == 2, "startup");
            double startupMs = started.Elapsed.TotalMilliseconds;
            Check(((Border)window.FindName("Preview")).Visibility == Visibility.Collapsed, "filename-only preview hidden");
            content.Text = "needle";
            await Until(() => list.Items.Count == 1 && hits.Items.Count > 0, "first useful hit");
            await Until(() => title.Text.Contains("50,000", StringComparison.Ordinal), "count all huge hits");
            Check(list.Items.Count == 1, "50000 hits produce one file row");
            Check(hits.Items.Count <= 32, "only a bounded hit page is materialized");
            object selectedFile = list.SelectedItem;
            long Before() => ((Hit)hits.SelectedItem).Ordinal;
            long before = Before();
            RaiseKey(window, hits, Key.Down);
            await Until(() => Before() == before + 1, "hit down");
            Check(ReferenceEquals(list.SelectedItem, selectedFile), "hit down keeps same file");
            RaiseKey(window, hits, Key.End);
            await Until(() => Before() == 49999, "hit end");
            Check(hits.Items.Count <= 32 && ReferenceEquals(list.SelectedItem, selectedFile), "end stays bounded and same file");
            RaiseKey(window, hits, Key.Home);
            await Until(() => Before() == 0, "hit home");
            RaiseKey(window, hits, Key.Up);
            await Until(() => Before() == 49999, "hit up wraps");
            Check(ReferenceEquals(list.SelectedItem, selectedFile), "up stays same file");
            content.Text = "needle"; content.Text = "日本語";
            await Until(() => list.Items.Count == 1 && ((ResultItem)list.Items[0]).Name == "日本語.txt" && hits.Items.Count > 0, "newest query");
            await Task.Delay(150);
            Check(((ResultItem)list.Items[0]).Name == "日本語.txt", "stale query never overwrites newest result");
            file.Text = "many";
            await Until(() => summary.Text.StartsWith("一致するファイルはありません", StringComparison.Ordinal), "AND zero result");
            Check(list.Items.Count == 0, "filename AND content");
            file.Clear(); mode.SelectedIndex = 1; content.Text = "[";
            await Until(() => summary.Text.StartsWith("検索エラー", StringComparison.Ordinal), "invalid regex displayed");
            Check(list.Items.Count == 0, "invalid expression has no stale results");
            mode.SelectedIndex = 0; content.Text = "needle";
            await Until(() => hits.Items.Count > 0 && list.Items.Count == 1, "recover valid query");
            window.UpdateLayout();
            var bitmap = new RenderTargetBitmap((int)window.ActualWidth, (int)window.ActualHeight, 96, 96, PixelFormats.Pbgra32);
            bitmap.Render(window);
            using (var output = File.Create(Path.Combine(report, "window.png")))
            { var encoder = new PngBitmapEncoder(); encoder.Frames.Add(BitmapFrame.Create(bitmap)); encoder.Save(output); }
            File.WriteAllText(Path.Combine(report, "result.json"), JsonSerializer.Serialize(new { pass = true, startupMs, checks = results,
                privateBytes = Process.GetCurrentProcess().PrivateMemorySize64, utc = DateTime.UtcNow }, JsonOptions));
            Console.WriteLine($"PASS GUI {results.Count} checks / startup {startupMs:F2}ms");
        }
        catch (Exception ex)
        {
            File.WriteAllText(Path.Combine(report, "result.json"), JsonSerializer.Serialize(new { pass = false, checks = results, error = ex.ToString() }, JsonOptions));
            throw;
        }
        finally
        {
            if (window != null) await CloseWindow(window);
            if (Directory.Exists(work)) Directory.Delete(work, true);
        }
    }

    private static async Task CloseWindow(MainWindow window)
    {
        if (!window.IsLoaded) return;
        var finished = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        window.Closed += (_, _) => finished.TrySetResult();
        window.Close();
        await finished.Task.WaitAsync(TimeSpan.FromSeconds(30));
    }

    private static void RaiseKey(MainWindow window, UIElement element, Key key)
    {
        var source = PresentationSource.FromVisual(window) ?? throw new InvalidOperationException("No presentation source");
        element.RaiseEvent(new KeyEventArgs(Keyboard.PrimaryDevice, source, Environment.TickCount, key)
        { RoutedEvent = Keyboard.PreviewKeyDownEvent });
    }

    private static async Task Until(Func<bool> condition, string name, TimeSpan? timeout = null)
    {
        var watch = Stopwatch.StartNew();
        TimeSpan limit = timeout ?? TimeSpan.FromSeconds(10);
        while (!condition()) { if (watch.Elapsed > limit) throw new Exception("Timeout: " + name); await Task.Delay(10); }
    }

    private static readonly JsonSerializerOptions JsonOptions = new() { WriteIndented = true };
}
