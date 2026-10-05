using System.ComponentModel;
using System.Diagnostics;
using Microsoft.Win32;
using Velopack.Locators;

namespace Zflow;
internal static class Installation
{
    public static string StartupExecutable => VelopackLocator.Current is { CurrentlyInstalledVersion: not null, IsPortable: false, RootAppDir: string root }
        ? Path.Combine(root, "Zflow.App.exe") : Environment.ProcessPath!;

    public static void RefreshStartupPath()
    {
        if (VelopackLocator.Current is not { CurrentlyInstalledVersion: not null, IsPortable: false }) return;
        using var run = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", writable: true);
        if (run?.GetValue("zflow") is not null) run.SetValue("zflow", $"\"{StartupExecutable}\" --background");
    }

    public static void RemoveStartupPath()
    {
        using var run = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", writable: true);
        if (run?.GetValue("zflow") is string command && command.StartsWith($"\"{StartupExecutable}\"", StringComparison.OrdinalIgnoreCase)) run.DeleteValue("zflow", false);
    }

    public static async Task StopLegacyInstallationAsync()
    {
        if (VelopackLocator.Current is not { CurrentlyInstalledVersion: not null, IsPortable: false }) return;
        string oldDirectory = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "zflow");
        foreach (string name in new[] { "Zflow.App", "zflow" })
        {
            foreach (var process in Process.GetProcessesByName(name))
            {
                using (process)
                {
                    if (process.Id == Environment.ProcessId) continue;
                    string? path;
                    try { path = process.MainModule?.FileName; }
                    catch (InvalidOperationException) { continue; }
                    catch (Win32Exception) { continue; } // The instance mutex still prevents a second settings app.
                    if (!string.Equals(path, Path.Combine(oldDirectory, name + ".exe"), StringComparison.OrdinalIgnoreCase)) continue;
                    if (name == "Zflow.App") NativeWindow.QuitExisting();
                    else await new EngineClient().StopAsync();
                    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
                    try { await process.WaitForExitAsync(timeout.Token); }
                    catch (OperationCanceledException) { throw new IOException("Quit the previous zflow from its notification-area menu, then open the new zflow again."); }
                }
            }
        }
    }
}
