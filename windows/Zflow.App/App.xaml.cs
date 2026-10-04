using Microsoft.UI.Xaml;
namespace Zflow;
public partial class App : Application
{
    private MainWindow? window;
    private Mutex? single;
    public App() { InitializeComponent(); }
    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        if (Environment.GetCommandLineArgs().Contains("--quit")) { NativeWindow.QuitExisting(); Exit(); return; }
        single = new Mutex(true, @"Local\Zflow.WinUI." + Environment.UserName, out bool created);
        if (!created) { NativeWindow.ShowExisting(); Exit(); return; }
        window = new MainWindow();
        window.Activate();
    }
}
