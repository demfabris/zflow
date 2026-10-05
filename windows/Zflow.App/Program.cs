using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Velopack;

namespace Zflow;
internal static class Program
{
    [STAThread]
    private static void Main()
    {
        // Installer hooks must finish before WinUI, the instance mutex, or input hooks start.
        VelopackApp.Build().SetAutoApplyOnStartup(false)
            .OnAfterInstallFastCallback(_ => Installation.RefreshStartupPath())
            .OnBeforeUninstallFastCallback(_ => Installation.RemoveStartupPath()).Run();
        WinRT.ComWrappersSupport.InitializeComWrappers();
        Application.Start(args =>
        {
            SynchronizationContext.SetSynchronizationContext(new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread()));
            _ = new App();
        });
    }
}
