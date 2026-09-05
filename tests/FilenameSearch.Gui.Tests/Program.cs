using System.IO;
using System.Diagnostics;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using PersonalRag.FilenameSearch.Gui;

internal static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        if (!OperatingSystem.IsWindows())
        {
            Console.WriteLine("SKIP FilenameSearch GUI tests: WPF requires Windows");
            return;
        }
        if (args.Length >= 4 && args[0].Equals("formal-startup", StringComparison.OrdinalIgnoreCase))
        {
            RunFormalStartup(args[1], args[2], args[3]);
            return;
        }
        RunFunctional();
    }

    private static void RunFunctional()
    {
        string work = Path.Combine(Path.GetTempPath(), "personalrag-filename-gui-tests-" + Guid.NewGuid().ToString("N"));
        string root = Path.Combine(work, "root");
        string store = Path.Combine(work, "store", "index.routec");
        Directory.CreateDirectory(root);
        File.WriteAllText(Path.Combine(root, "alpha.txt"), "filename");
        File.WriteAllText(Path.Combine(root, "beta.log"), "filename");
        int checks = 0;
        void Check(bool condition, string name) { checks++; if (!condition) throw new Exception("FAIL: " + name); }
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        MainWindow? window = null;
        try
        {
            app.Startup += async (_, _) =>
            {
                try
                {
                    window = new MainWindow(root, store) { Left = -10000, Top = -10000, ShowInTaskbar = false };
                    window.Show();
                    var results = (ListView)window.FindName("Results");
                    var query = (TextBox)window.FindName("FileQuery");
                    var content = (TextBox)window.FindName("ContentQuery");
                    var summary = (TextBlock)window.FindName("Summary");
                    await Until(() => results.Items.Count >= 2, "initial results");
                    Check(!content.IsEnabled, "content search is disabled in filename phase");
                    query.Text = "alpha";
                    await Until(() => results.Items.Count == 1, "filename query");
                    query.Text = "beta";
                    query.Text = "alpha";
                    await Until(() => results.Items.Count == 1 && ((ResultRow)results.Items[0]).Name == "alpha.txt" && summary.Text.Contains("件表示", StringComparison.Ordinal), "stale query suppression");
                    Check(summary.Text.Contains("件表示", StringComparison.Ordinal), "summary reports first useful batch: " + summary.Text);
                    Check(((ResultRow)results.Items[0]).FullPath.EndsWith("alpha.txt", StringComparison.OrdinalIgnoreCase), "path column is populated");
                    Console.WriteLine($"PASS FilenameSearch GUI {checks} checks");
                }
                finally
                {
                    if (window is not null) window.Close();
                    app.Shutdown();
                }
            };
            app.Run();
        }
        finally
        {
            try { if (Directory.Exists(work)) Directory.Delete(work, true); }
            catch (IOException) { }
        }
    }

    private static void RunFormalStartup(string rootArg, string storeArg, string reportArg)
    {
        string root = Path.GetFullPath(rootArg), store = Path.GetFullPath(storeArg), report = Path.GetFullPath(reportArg);
        Directory.CreateDirectory(Path.GetDirectoryName(report)!);
        var processWatch = Stopwatch.StartNew();
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        MainWindow? window = null;
        try
        {
            app.Startup += async (_, _) =>
            {
                try
                {
                    window = new MainWindow(root, store) { Left = -10000, Top = -10000, ShowInTaskbar = false };
                    window.Show();
                    var results = (ListView)window.FindName("Results");
                    var summary = (TextBlock)window.FindName("Summary");
                    await Until(() => results.Items.Count > 0 && summary.Text.Contains("件表示", StringComparison.Ordinal), "formal first batch", TimeSpan.FromSeconds(120));
                    processWatch.Stop();
                    using var process = Process.GetCurrentProcess();
                    process.Refresh();
                    long privateBytes = process.PrivateMemorySize64;
                    var output = new
                    {
                        pass = processWatch.Elapsed.TotalMilliseconds <= 2000 && privateBytes <= 1_500_000_000,
                        filename_ready_ms = processWatch.Elapsed.TotalMilliseconds,
                        private_bytes = privateBytes,
                        first_batch_rows = results.Items.Count,
                        root,
                        store,
                        utc = DateTime.UtcNow
                    };
                    File.WriteAllText(report, JsonSerializer.Serialize(output, new JsonSerializerOptions { WriteIndented = true }));
                    Console.WriteLine($"FORMAL Filename GUI {processWatch.Elapsed.TotalMilliseconds:F2}ms / {privateBytes} bytes");
                }
                catch (Exception ex)
                {
                    string summaryText = window is null ? "" : ((TextBlock)window.FindName("Summary")).Text;
                    string statusText = window is null ? "" : ((TextBlock)window.FindName("Status")).Text;
                    int resultCount = window is null ? 0 : ((ListView)window.FindName("Results")).Items.Count;
                    File.WriteAllText(report, JsonSerializer.Serialize(new { pass = false, error = ex.ToString(), summary = summaryText, status = statusText, resultCount, utc = DateTime.UtcNow }, new JsonSerializerOptions { WriteIndented = true }));
                    throw;
                }
                finally
                {
                    if (window is not null) window.Close();
                    app.Shutdown();
                }
            };
            app.Run();
        }
        finally { }
    }

    private static async Task Until(Func<bool> condition, string name, TimeSpan? timeout = null)
    {
        using var timeoutSource = new CancellationTokenSource(timeout ?? TimeSpan.FromSeconds(15));
        while (!condition())
        {
            timeoutSource.Token.ThrowIfCancellationRequested();
            await Task.Delay(20, timeoutSource.Token);
        }
    }
}
