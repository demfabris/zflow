using Velopack;
using Velopack.Sources;

namespace Zflow;
internal sealed class AppUpdates
{
    private readonly UpdateManager manager;
    private UpdateInfo? available;
    private VelopackAsset? downloaded;
    public bool IsEnabled => manager.IsInstalled && !manager.IsPortable;
    public bool IsBusy { get; private set; }
    public bool CanDownload => available is not null && downloaded is null;
    public bool CanRestart => downloaded is not null;
    public string? Version => (downloaded ?? available?.TargetFullRelease)?.Version.ToString();

    public AppUpdates(UpdateManager? manager = null)
    {
        this.manager = manager ?? new UpdateManager(new GithubSource("https://github.com/demfabris/zflow", null, false), new UpdateOptions { ExplicitChannel = "win-x64" });
        if (IsEnabled) downloaded = this.manager.UpdatePendingRestart;
    }

    public async Task CheckAsync()
    {
        if (!IsEnabled || IsBusy || downloaded is not null) return;
        IsBusy = true;
        try { available = await manager.CheckForUpdatesAsync(); }
        finally { IsBusy = false; }
    }

    public async Task DownloadAsync(Action<int> progress)
    {
        if (!IsEnabled || IsBusy || !CanDownload) return;
        IsBusy = true;
        try
        {
            var update = available!;
            await manager.DownloadUpdatesAsync(update, progress);
            downloaded = update.TargetFullRelease;
        }
        finally { IsBusy = false; }
    }

    public async Task PrepareRestartAsync(Func<Task> stopEngine, Action<VelopackAsset>? apply = null)
    {
        if (!IsEnabled || IsBusy || downloaded is null) throw new InvalidOperationException("Download the update before restarting zflow.");
        IsBusy = true;
        try
        {
            // Confirm that input has stopped before Update.exe can replace the engine.
            await stopEngine();
            if (apply is not null) apply(downloaded);
            else manager.WaitExitThenApplyUpdates(downloaded, silent: false, restart: true);
        }
        finally { IsBusy = false; }
    }
}
