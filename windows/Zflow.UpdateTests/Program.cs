using Velopack;
using Zflow;

static void Require(bool condition, string message)
{
    if (!condition) throw new Exception(message);
}

static async Task ExpectFailure(Func<Task> action)
{
    try { await action(); }
    catch (IOException) { return; }
    throw new Exception("Expected an I/O failure");
}

foreach (var manager in new[] { new FakeManager { Installed = false }, new FakeManager { Portable = true } })
{
    var unsupported = new AppUpdates(manager);
    await unsupported.CheckAsync();
    Require(!unsupported.IsEnabled && manager.CheckCount == 0, "Development and portable builds must not contact the update feed");
}

var fake = new FakeManager();
var updates = new AppUpdates(fake);
var gate = new TaskCompletionSource<UpdateInfo?>();
fake.Check = () => gate.Task;
var check = updates.CheckAsync();
await updates.CheckAsync();
Require(updates.IsBusy && fake.CheckCount == 1, "Concurrent checks must share one operation");
gate.SetResult(FakeManager.NewRelease);
await check;
Require(updates.CanDownload && updates.Version == "0.6.0", "A newer release must be available to download");
fake.DownloadFails = true;
await ExpectFailure(() => updates.DownloadAsync(_ => { }));
Require(updates.CanDownload && !updates.CanRestart && !updates.IsBusy, "A failed download must stay retryable and must not allow installation");
fake.DownloadFails = false;
await updates.DownloadAsync(_ => { });
Require(updates.CanRestart && !updates.CanDownload, "Only a completed download can restart");
bool applied = false;
await ExpectFailure(() => updates.PrepareRestartAsync(() => throw new IOException("Engine still running"), _ => applied = true));
Require(!applied && updates.CanRestart && !updates.IsBusy, "The updater must not start when input-engine shutdown fails");
var order = new List<string>();
await updates.PrepareRestartAsync(() => { order.Add("engine stopped"); return Task.CompletedTask; }, _ => order.Add("updater started"));
Require(order.SequenceEqual(new[] { "engine stopped", "updater started" }), "The engine must stop before the updater starts");

var pending = new AppUpdates(new FakeManager { Pending = FakeManager.NewRelease.TargetFullRelease });
Require(pending.CanRestart && pending.Version == "0.6.0", "A downloaded update must remain available after relaunch");
await pending.CheckAsync();
Require(pending.CanRestart, "A background check must not discard a downloaded update");
Console.WriteLine("Windows update lifecycle checks passed.");

sealed class FakeManager : UpdateManager
{
    public static UpdateInfo NewRelease => new(new VelopackAsset { PackageId = "Zflow.App", Version = SemanticVersion.Parse("0.6.0"), Type = VelopackAssetType.Full, FileName = "Zflow.App-0.6.0-full.nupkg" }, false);
    public bool Installed = true, Portable, DownloadFails;
    public int CheckCount;
    public VelopackAsset? Pending;
    public Func<Task<UpdateInfo?>> Check = () => Task.FromResult<UpdateInfo?>(NewRelease);
    public FakeManager() : base("https://example.invalid", locator: new Velopack.Locators.TestVelopackLocator("Zflow.App", "0.5.0", Path.GetTempPath(), Path.GetTempPath(), Path.GetTempPath(), "unused", "win-x64")) { }
    public override bool IsInstalled => Installed;
    public override bool IsPortable => Portable;
    public override VelopackAsset? UpdatePendingRestart => Pending;
    public override Task<UpdateInfo?> CheckForUpdatesAsync() { CheckCount++; return Check(); }
    public override Task DownloadUpdatesAsync(UpdateInfo updates, Action<int>? progress = null, CancellationToken cancelToken = default)
    {
        if (DownloadFails) throw new IOException("Download interrupted");
        progress?.Invoke(100);
        return Task.CompletedTask;
    }
}
