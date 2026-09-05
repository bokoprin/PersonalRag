using System.IO;
using System.Windows;
using System.Windows.Controls;
using PersonalRag.FilenameSearch.Gui;

internal static class Program
{
    [STAThread]
    private static void Main()
    {
        if (!OperatingSystem.IsWindows())
        {
            Console.WriteLine("SKIP FilenameSearch GUI tests: WPF requires Windows");
            return;
        }
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

    private static async Task Until(Func<bool> condition, string name)
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
        while (!condition())
        {
            timeout.Token.ThrowIfCancellationRequested();
            await Task.Delay(20, timeout.Token);
        }
    }
}
