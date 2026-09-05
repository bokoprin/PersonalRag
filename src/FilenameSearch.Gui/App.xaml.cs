using System.Windows;

namespace PersonalRag.FilenameSearch.Gui;

public partial class App : Application
{
    private void OnStartup(object sender, StartupEventArgs e)
    {
        string? root = e.Args.Length > 0 ? e.Args[0] : Environment.GetEnvironmentVariable("PERSONALRAG_ROOT");
        string? store = e.Args.Length > 1 ? e.Args[1] : null;
        var window = new MainWindow(root, store);
        MainWindow = window;
        window.Show();
    }
}
