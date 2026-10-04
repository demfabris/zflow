using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Security.Principal;

namespace Zflow;
internal static class Elevation
{
    public static bool IsElevated
    {
        get { using var identity = WindowsIdentity.GetCurrent(); return new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator); }
    }
    private static string UserSid
    {
        get { using var identity = WindowsIdentity.GetCurrent(); return identity.User!.Value; }
    }

    public static async Task<bool> RequestRestartAsync()
    {
        string readyName = @"Local\Zflow.Restart." + Guid.NewGuid().ToString("N");
        using var ready = new EventWaitHandle(false, EventResetMode.ManualReset, readyName);
        var start = new ProcessStartInfo(Environment.ProcessPath!) { UseShellExecute = true, Verb = "runas" };
        start.ArgumentList.Add("--replace-process"); start.ArgumentList.Add(Environment.ProcessId.ToString());
        start.ArgumentList.Add("--replace-user"); start.ArgumentList.Add(UserSid);
        start.ArgumentList.Add("--restart-ready"); start.ArgumentList.Add(readyName);
        try
        {
            using var child = Process.Start(start);
            if (child is null) return false;
            // Keep the current app if elevation used another account or the
            // replacement could not initialize. Only the child signals readiness.
            if (!await Task.Run(() => ready.WaitOne(TimeSpan.FromSeconds(8))))
                throw new IOException("The administrator app could not start. Your current zflow is still running.");
            return true;
        }
        catch (Win32Exception e) when (e.NativeErrorCode == 1223) { return false; } // UAC cancelled: keep the current app.
    }

    public static async Task<bool> WaitForPreviousAsync(string[] args)
    {
        int replace = Array.IndexOf(args, "--replace-process");
        if (replace < 0) return true;
        try
        {
            int user = Array.IndexOf(args, "--replace-user");
            if (user < 0 || user + 1 >= args.Length || args[user + 1] != UserSid)
                throw new InvalidOperationException("Restart using the same Windows account to keep your computers and settings.");
            if (!IsElevated || replace + 1 >= args.Length || !int.TryParse(args[replace + 1], out int pid) || pid == Environment.ProcessId)
                throw new InvalidOperationException("Could not restart zflow as administrator.");
            int readyArg = Array.IndexOf(args, "--restart-ready");
            if (readyArg < 0 || readyArg + 1 >= args.Length)
                throw new InvalidOperationException("The restart request is incomplete.");
            using (var ready = EventWaitHandle.OpenExisting(args[readyArg + 1])) ready.Set();
            Process previous;
            try { previous = Process.GetProcessById(pid); }
            catch (ArgumentException) { return true; } // The previous app already exited.
            using (previous)
            using (var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20)))
                await previous.WaitForExitAsync(timeout.Token);
            return true;
        }
        catch (Exception e)
        {
            MessageBox(0, e is OperationCanceledException ? "The previous zflow app did not stop. Quit it and try again." : e.Message, "zflow", 0x10);
            return false;
        }
    }
    [DllImport("user32", CharSet = CharSet.Unicode, EntryPoint = "MessageBoxW")]
    private static extern int MessageBox(nint owner, string text, string caption, uint type);
}
