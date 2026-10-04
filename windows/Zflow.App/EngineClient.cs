using System.Buffers.Binary;
using System.Diagnostics;
using System.IO.Pipes;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Zflow;
internal sealed class EngineClient
{
    public static string DataDirectory => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "zflow");
    public static string ConfigPath => Path.Combine(DataDirectory, "zflow.toml");
    private static string PipeName => "zflow-" + Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(Path.GetFullPath(ConfigPath).ToLowerInvariant())))[..24].ToLowerInvariant();
    private Process? process;
    private readonly Queue<string> recentErrors = new();

    public async Task<JsonObject> SendAsync(object request, int timeoutMs = 12000)
    {
        using var timeout = new CancellationTokenSource(timeoutMs);
        await using var pipe = new NamedPipeClientStream(".", PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
        await pipe.ConnectAsync(timeout.Token);
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(request);
        if (body.Length > 65536) throw new InvalidOperationException("The request is too large.");
        byte[] header = new byte[4]; BinaryPrimitives.WriteInt32BigEndian(header, body.Length);
        await pipe.WriteAsync(header, timeout.Token); await pipe.WriteAsync(body, timeout.Token); await pipe.FlushAsync(timeout.Token);
        await pipe.ReadExactlyAsync(header, timeout.Token);
        int length = BinaryPrimitives.ReadInt32BigEndian(header);
        if (length is < 0 or > 65536) throw new IOException("Invalid response from the input engine.");
        body = new byte[length]; await pipe.ReadExactlyAsync(body, timeout.Token);
        var result = JsonNode.Parse(body)?.AsObject() ?? throw new IOException("The engine returned no state.");
        if (result["error"] is JsonValue error) throw new InvalidOperationException(error.GetValue<string>());
        return result;
    }

    public async Task<JsonObject> StartAsync()
    {
        try { return await SendAsync(new { command = "status" }, 300); }
        catch (Exception e) when (e is IOException or OperationCanceledException or TimeoutException) { }
        string executable = Path.Combine(AppContext.BaseDirectory, "zflow.exe");
        if (!File.Exists(executable)) throw new FileNotFoundException("The Windows input engine is missing. Build or reinstall the complete zflow app.", executable);
        Directory.CreateDirectory(DataDirectory);
        var start = new ProcessStartInfo(executable) { UseShellExecute = false, CreateNoWindow = true, RedirectStandardError = true, RedirectStandardOutput = true };
        start.ArgumentList.Add("--config"); start.ArgumentList.Add(ConfigPath); start.ArgumentList.Add("run");
        process = new Process { StartInfo = start };
        process.ErrorDataReceived += (_, e) => { if (e.Data is not null) lock (recentErrors) { recentErrors.Enqueue(e.Data); while (recentErrors.Count > 12) recentErrors.Dequeue(); } };
        process.Start(); process.BeginErrorReadLine(); process.BeginOutputReadLine();
        for (int attempt = 0; attempt < 40; ++attempt)
        {
            if (process.HasExited) { lock (recentErrors) throw new IOException("Input engine could not start. " + string.Join("\n", recentErrors)); }
            try { return await SendAsync(new { command = "status" }, 400); }
            catch (Exception e) when (e is IOException or OperationCanceledException or TimeoutException) { await Task.Delay(100); }
        }
        throw new IOException("The input engine did not start. Check that UDP port 43119 is available.");
    }
}
