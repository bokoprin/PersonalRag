using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using Astra.Core;
using Microsoft.Win32;

namespace Astra.Gui;

public partial class MainWindow : Window
{
    private readonly ObservableCollection<ResultItem> rows = [];
    private readonly string store;
    private IndexRuntime? runtime;
    private SearchEngine? engine;
    private CancellationTokenSource queryCancellation = new(), hitCancellation = new();
    private long queryVersion, selectionVersion, activeHit, totalHits;
    private int nextOffset;
    private SearchRequest current = new();
    private bool ready, closing, closed;
    public MainWindow() : this(null) { }
    public MainWindow(string? storeOverride)
    {
        store = storeOverride ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "PersonalRagAstra", "index");
        InitializeComponent(); Results.ItemsSource = rows;
    }
    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        ready = true; FileQuery.Focus();
        if (File.Exists(Path.Combine(store, "snapshot.astra")))
        {
            try { await StartRuntime(await Task.Run(() => IndexStore.Load(store))); }
            catch (Exception ex) { Status.Text = "インデックスを利用できません: " + ex.Message; }
        }
        else Status.Text = "対象フォルダーを選択してください";
    }
    private async Task StartRuntime(IndexSnapshot snapshot)
    {
        if (runtime != null) await runtime.DisposeAsync();
        runtime = new IndexRuntime(store, snapshot);
        runtime.Changed += () => Dispatcher.BeginInvoke(() =>
        {
            if (closing || runtime is null) return;
            Status.Text = $"{runtime.Status} · {runtime.Snapshot.Files.Length:N0} files";
            if (!ReferenceEquals(engine?.Snapshot, runtime.Snapshot)) { engine = new SearchEngine(runtime.Snapshot); _ = Search(false); }
        });
        engine = new SearchEngine(snapshot); Status.Text = $"Ready · {snapshot.Files.Length:N0} files";
        await Search(false);
    }
    private async void ChooseFolder(object sender, RoutedEventArgs e)
    {
        var dialog = new OpenFolderDialog { Title = "検索するフォルダー" };
        if (dialog.ShowDialog(this) != true) return;
        try
        {
            Status.Text = "インデックスを構築中";
            ((Button)sender).IsEnabled = false;
            var snapshot = await Task.Run(() => new IndexBuilder().Build(dialog.FolderName, progress: n => Dispatcher.BeginInvoke(() => Status.Text = $"構築中 · {n:N0} files")));
            if (runtime != null) { await runtime.DisposeAsync(); runtime = null; }
            await Task.Run(() => IndexStore.Save(store, snapshot));
            await StartRuntime(snapshot);
        }
        catch (Exception ex) { Status.Text = "構築エラー: " + ex.Message; }
        finally { ((Button)sender).IsEnabled = true; }
    }
    private async void QueryChanged(object sender, RoutedEventArgs e) { if (ready) await Search(false); }
    private SearchRequest ReadRequest() => new(FileQuery.Text, ContentQuery.Text, Scope.SelectedIndex == 1 ? FileScope.FullPath : FileScope.Filename,
        (ContentMode)Math.Max(0, Mode.SelectedIndex), CaseSensitive.IsChecked == true);
    private async Task Search(bool more)
    {
        queryCancellation.Cancel(); queryCancellation.Dispose(); queryCancellation = new(); var token = queryCancellation.Token;
        long version = ++queryVersion; hitCancellation.Cancel(); selectionVersion++;
        if (!more) { current = ReadRequest(); nextOffset = 0; rows.Clear(); HitList.ItemsSource = null; PreviewTitle.Text = ""; }
        Preview.Visibility = current.ContentQuery.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        HitsColumn.Width = current.ContentQuery.Length > 0 ? 70 : 0; More.IsEnabled = false;
        if (engine is null) return;
        var searchEngine = engine; var request = current; int offset = nextOffset;
        Summary.Text = "検索中…";
        try
        {
            var page = await Task.Run(() => searchEngine.Search(request, offset, 100, token), token);
            if (version != queryVersion || closing) return;
            foreach (var row in page.Rows) rows.Add(new ResultItem(row));
            nextOffset = page.NextOffset; More.IsEnabled = !page.Complete;
            Summary.Text = $"{rows.Count:N0}件表示 · {page.ElapsedMs:F1} ms" + (page.Complete ? "" : " · さらに候補あり") +
                (page.Warnings.Count > 0 ? $" · 検索不能/エラー {page.Warnings.Count}件以上" : "");
            if (rows.Count == 0 && page.Complete) Summary.Text = "一致するファイルはありません。" + (page.Warnings.Count > 0 ? " 検索不能ファイルがあります。" : "");
            if (!more && rows.Count > 0 && current.ContentQuery.Length > 0) Results.SelectedIndex = 0;
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { if (version == queryVersion) Summary.Text = "検索エラー: " + ex.Message; }
    }
    private async void ShowMore(object sender, RoutedEventArgs e) => await Search(true);
    private void ClearContent(object sender, RoutedEventArgs e) { ContentQuery.Clear(); ContentQuery.Focus(); }
    private async void SelectedFileChanged(object sender, SelectionChangedEventArgs e)
    {
        if (Results.SelectedItem is not ResultItem row || current.ContentQuery.Length == 0 || engine is null) return;
        hitCancellation.Cancel(); hitCancellation.Dispose(); hitCancellation = new(); var token = hitCancellation.Token;
        long version = ++selectionVersion; activeHit = 0; totalHits = 0;
        var selectedEngine = engine; var request = current;
        PreviewTitle.Text = row.Name + " · 一致を読み込み中";
        try
        {
            var page = await Task.Run(() => selectedEngine.GetHits(row.Row.File.Path, request, cancellationToken: token), token);
            if (version != selectionVersion || closing) return;
            HitList.ItemsSource = page.Hits; HitList.SelectedIndex = 0;
            PreviewTitle.Text = $"{row.Name} · {page.Total:N0}{(page.Complete ? "" : "+")} matches";
            var count = page.Complete ? page : await Task.Run(() => selectedEngine.GetHits(row.Row.File.Path, request, limit: 1, countAll: true, cancellationToken: token), token);
            if (version != selectionVersion || closing) return;
            totalHits = count.Total; row.SetHits(totalHits); PreviewTitle.Text = $"{row.Name} · {totalHits:N0} matches";
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { if (version == selectionVersion) PreviewTitle.Text = "読込エラー: " + ex.Message; }
    }
    private async Task NavigateHit(long target)
    {
        if (Results.SelectedItem is not ResultItem row || engine == null || totalHits == 0) return;
        target = (target % totalHits + totalHits) % totalHits;
        activeHit = target;
        if (HitList.ItemsSource is IReadOnlyList<Hit> cached && cached.FirstOrDefault(h => h.Ordinal == target) is { } hit)
        { HitList.SelectedItem = hit; HitList.ScrollIntoView(hit); return; }
        long version = selectionVersion; var token = hitCancellation.Token; var selectedEngine = engine; var request = current;
        try
        {
            var page = await Task.Run(() => selectedEngine.GetHits(row.Row.File.Path, request, target, cancellationToken: token), token);
            if (version != selectionVersion || activeHit != target || closing) return;
            HitList.ItemsSource = page.Hits; HitList.SelectedIndex = 0; HitList.ScrollIntoView(HitList.SelectedItem);
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { if (version == selectionVersion) PreviewTitle.Text = "一致読込エラー: " + ex.Message; }
    }
    private async void HitKey(object sender, KeyEventArgs e)
    {
        if (e.Key is Key.Up or Key.Down or Key.Home or Key.End)
        {
            e.Handled = true;
            await NavigateHit(e.Key switch { Key.Up => activeHit - 1, Key.Down => activeHit + 1, Key.Home => 0, _ => totalHits - 1 });
        }
    }
    private void HitSelected(object sender, SelectionChangedEventArgs e) { if (HitList.SelectedItem is Hit hit) activeHit = hit.Ordinal; }
    private async void PreviousHit(object sender, RoutedEventArgs e) => await NavigateHit(activeHit - 1);
    private async void NextHit(object sender, RoutedEventArgs e) => await NavigateHit(activeHit + 1);
    private void ResultKey(object sender, KeyEventArgs e) { if (e.Key == Key.Enter) { e.Handled = true; OpenFile(); } }
    private void OpenSelected(object sender, MouseButtonEventArgs e) => OpenFile();
    private void OpenFile()
    {
        if (Results.SelectedItem is not ResultItem row) return;
        try { Process.Start(new ProcessStartInfo(row.Row.File.Path) { UseShellExecute = true }); }
        catch (Exception ex) { Status.Text = "ファイルを開けません: " + ex.Message; }
    }
    private void OnGlobalKey(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.F && Keyboard.Modifiers.HasFlag(ModifierKeys.Control))
        { var input = Keyboard.Modifiers.HasFlag(ModifierKeys.Shift) ? ContentQuery : FileQuery; input.Focus(); input.SelectAll(); e.Handled = true; }
        else if (e.Key == Key.Escape && Keyboard.FocusedElement is TextBox input) { input.Clear(); e.Handled = true; }
    }
    private async void OnClosing(object? sender, System.ComponentModel.CancelEventArgs e)
    {
        if (closed) return;
        e.Cancel = true; if (closing) return; closing = true;
        queryCancellation.Cancel(); hitCancellation.Cancel();
        if (runtime != null) await runtime.DisposeAsync();
        closed = true; Close();
    }
}

public sealed class ResultItem(SearchRow row) : System.ComponentModel.INotifyPropertyChanged
{
    public SearchRow Row { get; } = row;
    public string Name => Path.GetFileName(Row.File.Path);
    public string Directory => Path.GetDirectoryName(Row.File.Path) ?? "";
    public string Size => $"{Row.File.Size:N0} B";
    public string Modified => new DateTime(Row.File.ModifiedUtcTicks, DateTimeKind.Utc).ToLocalTime().ToString("yyyy/MM/dd HH:mm");
    public string Hits { get; private set; } = row.HitsComplete ? row.Hits.ToString("N0") : row.Hits.ToString("N0") + "+";
    public event System.ComponentModel.PropertyChangedEventHandler? PropertyChanged;
    public void SetHits(long count) { Hits = count.ToString("N0"); PropertyChanged?.Invoke(this, new(nameof(Hits))); }
}
