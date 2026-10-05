using Microsoft.UI.Xaml;
namespace Zflow;
public partial class App : Application
{
    private MainWindow? window;
    private Mutex? single;
    public App() { InitializeComponent(); }
    protected override async void OnLaunched(LaunchActivatedEventArgs args)
    {
        if (Environment.GetCommandLineArgs().Contains("--quit")) { NativeWindow.QuitExisting(); Exit(); return; }
        if (!await Elevation.WaitForPreviousAsync(Environment.GetCommandLineArgs())) { Exit(); return; }
        try { await Installation.StopLegacyInstallationAsync(); }
        catch (Exception error) { NativeWindow.ShowStartupError(error.Message); Exit(); return; }
        bool created;
        try { single = new Mutex(true, @"Local\Zflow.WinUI." + Environment.UserName, out created); }
        catch (UnauthorizedAccessException) { NativeWindow.ShowExisting(); Exit(); return; } // Already running elevated.
        if (!created) { NativeWindow.ShowExisting(); Exit(); return; }
        Installation.RefreshStartupPath();
        window = new MainWindow();
        window.Activate();
    }
}
