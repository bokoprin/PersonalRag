using System.Diagnostics;
using System.IO;
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
        var application = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        int exit = 0;
        application.Startup += async (_, _) =>
        {
            string work = Path.Combine(Path.GetTempPath(), "astra-gui-tests-" + Guid.NewGuid().ToString("N"));
            string report = Path.GetFullPath(args.Length > 0 ? args[0] : "artifacts/gui-tests");
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
                Check(((System.Windows.Controls.Border)window.FindName("Preview")).Visibility == Visibility.Collapsed, "filename-only preview hidden");
                content.Text = "needle";
                await Until(() => list.Items.Count == 1 && hits.Items.Count > 0, "first useful hit");
                await Until(() => title.Text.Contains("50,000"), "count all huge hits");
                Check(list.Items.Count == 1, "50000 hits produce one file row");
                Check(hits.Items.Count <= 32, "only a bounded hit page is materialized");
                object selectedFile = list.SelectedItem;
                long Before() => ((Hit)hits.SelectedItem).Ordinal;
                long before = Before();
                Key(window, hits, System.Windows.Input.Key.Down);
                await Until(() => Before() == before + 1, "hit down");
                Check(ReferenceEquals(list.SelectedItem, selectedFile), "hit down keeps same file");
                Key(window, hits, System.Windows.Input.Key.End);
                await Until(() => Before() == 49999, "hit end");
                Check(hits.Items.Count <= 32 && ReferenceEquals(list.SelectedItem, selectedFile), "end stays bounded and same file");
                Key(window, hits, System.Windows.Input.Key.Home);
                await Until(() => Before() == 0, "hit home");
                Key(window, hits, System.Windows.Input.Key.Up);
                await Until(() => Before() == 49999, "hit up wraps");
                Check(ReferenceEquals(list.SelectedItem, selectedFile), "up stays same file");
                content.Text = "needle"; content.Text = "日本語";
                await Until(() => list.Items.Count == 1 && ((ResultItem)list.Items[0]).Name == "日本語.txt" && hits.Items.Count > 0, "newest query");
                await Task.Delay(150);
                Check(((ResultItem)list.Items[0]).Name == "日本語.txt", "stale query never overwrites newest result");
                file.Text = "many";
                await Until(() => summary.Text.StartsWith("一致するファイルはありません"), "AND zero result");
                Check(list.Items.Count == 0, "filename AND content");
                file.Clear(); mode.SelectedIndex = 1; content.Text = "[";
                await Until(() => summary.Text.StartsWith("検索エラー"), "invalid regex displayed");
                Check(list.Items.Count == 0, "invalid expression has no stale results");
                mode.SelectedIndex = 0; content.Text = "needle";
                await Until(() => hits.Items.Count > 0 && list.Items.Count == 1, "recover valid query");
                window.UpdateLayout();
                var bitmap = new RenderTargetBitmap((int)window.ActualWidth, (int)window.ActualHeight, 96, 96, PixelFormats.Pbgra32);
                bitmap.Render(window);
                using (var output = File.Create(Path.Combine(report, "window.png")))
                { var encoder = new PngBitmapEncoder(); encoder.Frames.Add(BitmapFrame.Create(bitmap)); encoder.Save(output); }
                File.WriteAllText(Path.Combine(report, "result.json"), JsonSerializer.Serialize(new { pass = true, startupMs, checks = results,
                    privateBytes = Process.GetCurrentProcess().PrivateMemorySize64, utc = DateTime.UtcNow }, new JsonSerializerOptions { WriteIndented = true }));
                Console.WriteLine($"PASS GUI {results.Count} checks / startup {startupMs:F2}ms");
            }
            catch (Exception ex)
            {
                exit = 1; Console.Error.WriteLine(ex);
                File.WriteAllText(Path.Combine(report, "result.json"), JsonSerializer.Serialize(new { pass = false, checks = results, error = ex.ToString() }));
            }
            finally
            {
                if (window != null)
                {
                    var finished = new TaskCompletionSource(); window.Closed += (_, _) => finished.TrySetResult();
                    window.Close(); await finished.Task.WaitAsync(TimeSpan.FromSeconds(15));
                }
                Directory.Delete(work, true);
                application.Shutdown(exit);
            }
        };
        application.Run(); return exit;
    }
    private static void Key(MainWindow window, UIElement element, Key key)
    {
        var source = PresentationSource.FromVisual(window) ?? throw new InvalidOperationException("No presentation source");
        element.RaiseEvent(new KeyEventArgs(Keyboard.PrimaryDevice, source, Environment.TickCount, key)
        { RoutedEvent = Keyboard.PreviewKeyDownEvent });
    }
    private static async Task Until(Func<bool> condition, string name)
    {
        var watch = Stopwatch.StartNew();
        while (!condition()) { if (watch.Elapsed.TotalSeconds > 10) throw new Exception("Timeout: " + name); await Task.Delay(10); }
    }
}
