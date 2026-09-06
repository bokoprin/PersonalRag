using System.IO;
using System.Windows;

namespace PersonalRag.FilenameSearch.Gui;

public partial class App : Application
{
    private void OnStartup(object sender, StartupEventArgs e)
    {
        string? root = Environment.GetEnvironmentVariable("PERSONALRAG_ROOT");
        string? store = null;
        string? probe = null;
        string? probeQuery = null;
        string? probeMode = null;
        bool exitAfterProbe = false;

        if (e.Args.Length > 0 && (e.Args[0].Equals("--startup-probe", StringComparison.OrdinalIgnoreCase) ||
                                  e.Args[0].Equals("--warm-probe", StringComparison.OrdinalIgnoreCase) ||
                                  e.Args[0].Equals("--live-probe", StringComparison.OrdinalIgnoreCase)))
        {
            if (e.Args.Length < 2) throw new ArgumentException("--startup-probe|--warm-probe|--live-probe REPORT [ROOT] [STORE]");
            probe = Path.GetFullPath(e.Args[1]);
            exitAfterProbe = true;
            probeMode = e.Args[0].Equals("--warm-probe", StringComparison.OrdinalIgnoreCase) ? "warm" :
                e.Args[0].Equals("--live-probe", StringComparison.OrdinalIgnoreCase) ? "live" : "startup";
            if (e.Args.Length > 2 && e.Args[2] != "-") root = e.Args[2];
            if (e.Args.Length > 3) store = e.Args[3];
            if (e.Args.Length > 4) probeQuery = e.Args[4];
        }
        else
        {
            if (e.Args.Length > 0) root = e.Args[0];
            if (e.Args.Length > 1) store = e.Args[1];
        }

        var window = new MainWindow(root, store, probe, exitAfterProbe, probeQuery, probeMode);
        MainWindow = window;
        window.Show();
    }
}
