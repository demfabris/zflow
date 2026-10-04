using System.Diagnostics;
using System.Text.Json.Nodes;
using System.Runtime.InteropServices.WindowsRuntime;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.Win32;
using Windows.Foundation;

namespace Zflow;
public sealed partial class MainWindow : Window
{
    private readonly EngineClient engine = new();
    private readonly DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(2) };
    private readonly NativeWindow native;
    private JsonObject? state;
    private bool refreshing, applying, dragging, quitting;
    private string previousView = "";
    private double scale = 0.1;
    public MainWindow()
    {
        InitializeComponent();
        Navigation.SelectedItem = ComputersNav;
        VersionText.Text = typeof(App).Assembly.GetName().Version?.ToString(3) ?? "";
        MessageBar.Closed += (_, _) => MessageBar.Visibility = Visibility.Collapsed;
        SystemBackdrop = new MicaBackdrop();
        ExtendsContentIntoTitleBar = true; SetTitleBar(TitleArea);
        string iconPath = Path.Combine(AppContext.BaseDirectory, "Assets", "zflow.ico");
        AppWindow.SetIcon(iconPath);
        native = new NativeWindow(this, iconPath);
        native.QuitRequested += async () => await QuitAsync();
        AppWindow.Closing += (_, args) => { if (!quitting) { args.Cancel = true; AppWindow.Hide(); } };
        timer.Tick += async (_, _) => await RefreshAsync();
        Root.Loaded += async (_, _) =>
        {
            // AppWindow uses physical pixels; size the settings UI consistently at any DPI.
            double dpi = Root.XamlRoot.RasterizationScale;
            var area = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
            int width = Math.Min((int)(1120 * dpi), area.Width - 48), height = Math.Min((int)(820 * dpi), area.Height - 48);
            AppWindow.MoveAndResize(new Windows.Graphics.RectInt32(area.X + (area.Width - width) / 2, area.Y + (area.Height - height) / 2, width, height));
            applying = true;
            using (var run = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run")) StartupSwitch.IsOn = run?.GetValue("zflow") is not null;
            applying = false;
            await StartAsync(); timer.Start();
            if (Environment.GetCommandLineArgs().Contains("--background")) AppWindow.Hide();
            var args = Environment.GetCommandLineArgs();
            int capture = Array.IndexOf(args, "--capture-preview");
            if (capture >= 0 && capture + 1 < args.Length) await CapturePreviewAsync(args[capture + 1]);
        };
    }
    private async Task StartAsync() { try { Update(await engine.StartAsync()); } catch (Exception e) { ShowError(e); } }
    private async Task RefreshAsync()
    {
        if (refreshing || dragging || applying || quitting) return;
        refreshing = true;
        try { Update(await engine.SendAsync(new { command = "status" }, 1500)); }
        catch (Exception e) { ConnectionSummary.Text = "Input engine is not responding"; SharingSwitch.IsEnabled = false; ShowError(e); }
        finally { refreshing = false; }
    }
    private async Task SendAsync(object request)
    {
        if (applying) return;
        applying = true;
        try { Update(await engine.SendAsync(request)); MessageBar.IsOpen = false; }
        catch (Exception e) { ShowError(e); }
        finally { applying = false; }
    }
    private void ShowError(Exception e) { MessageBar.Title = "zflow needs attention"; MessageBar.Message = e is OperationCanceledException ? "The engine did not answer. Restart it under Settings → Connection and troubleshooting." : e.Message; MessageBar.Visibility = Visibility.Visible; MessageBar.IsOpen = true; }
    private static string Text(JsonNode? n, string fallback = "") => n?.GetValue<string>() ?? fallback;
    private static bool Flag(JsonNode? n) => n?.GetValue<bool>() ?? false;
    private static int Number(JsonNode? n) => n?.GetValue<int>() ?? 0;
    private void Update(JsonObject next)
    {
        state = next;
        bool wasApplying = applying; applying = true;
        try
        {
            SharingSwitch.IsEnabled = true; SharingSwitch.IsOn = Flag(next["sharing"]);
            ClipboardSwitch.IsOn = Flag(next["clipboard"]); PauseEdgesSwitch.IsOn = Flag(next["pause_at_edges"]);
            LocalName.Text = Text(next["name"], "This computer"); LocalMark.Text = "Mark  " + Text(next["mark"]);
            string sending = Text(next["sending"]), receiving = Text(next["receiving"]);
            int connected = next["peers"]?.AsArray().Count(p => Flag(p?["connected"])) ?? 0;
            ConnectionSummary.Text = sending != "" ? $"Controlling {sending}" : receiving != "" ? $"Controlled by {receiving}" : !Flag(next["available"]) ? "Waiting for an unlocked Windows desktop" : !SharingSwitch.IsOn ? "Sharing paused · your input stays here" : $"Ready to share · {connected} computer{(connected == 1 ? "" : "s")} connected";
            NetworkDetails.Text = "Listening on " + Text(next["listen"]) + "\n" + string.Join("\n", next["addresses"]?.AsArray().Select(a => Text(a)) ?? []);
            string view = new JsonObject { ["peers"] = next["peers"]?.DeepClone(), ["nearby"] = next["nearby"]?.DeepClone(), ["layout"] = next["layout"]?.DeepClone() }.ToJsonString();
            if (view != previousView && !dragging) { previousView = view; DrawArrangement(); DrawPeers(); DrawNearby(); }
            if (next["notice"] is JsonValue notice) { MessageBar.Title = "Connection notice"; MessageBar.Message = notice.GetValue<string>(); MessageBar.Visibility = Visibility.Visible; MessageBar.IsOpen = true; }
        }
        finally { applying = wasApplying; }
    }
    private void DrawArrangement()
    {
        if (dragging || Arrangement.ActualWidth <= 64 || state?["layout"]?["monitors"] is not JsonArray allMonitors) return;
        Arrangement.Children.Clear();
        var monitors = allMonitors.Where(m => m?["display"]?["active"] is null || Flag(m["display"]!["active"])).ToArray();
        if (monitors.Length == 0) return;
        double width = Arrangement.ActualWidth, height = Arrangement.Height;
        int left = monitors.Min(m => Number(m?["x"])), top = monitors.Min(m => Number(m?["y"]));
        int right = monitors.Max(m => Number(m?["x"]) + Number(m?["width"])), bottom = monitors.Max(m => Number(m?["y"]) + Number(m?["height"]));
        scale = Math.Min((width - 64) / Math.Max(right - left, 1), (height - 64) / Math.Max(bottom - top, 1));
        double ox = (width - (right - left) * scale) / 2, oy = (height - (bottom - top) * scale) / 2;
        foreach (var node in monitors)
        {
            if (node is not JsonObject m) continue;
            bool local = m["peer"] is null;
            var stack = new StackPanel { Spacing = 6, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
            if (Number(m["height"]) * scale >= 80) stack.Children.Add(new FontIcon { Glyph = local ? "\uE7F8" : "\uE7F4", FontSize = 22 });
            stack.Children.Add(new TextBlock { Text = Text(m["display"]?["name"], Text(m["label"])), FontSize = 13, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, HorizontalAlignment = HorizontalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = Math.Max(10, Number(m["width"]) * scale - 20) });
            if (m["display"] is not null && Number(m["height"]) * scale >= 66) stack.Children.Add(new TextBlock { Text = local ? "This PC" : Text(m["peer"]), FontSize = 12, Foreground = Brush("TextFillColorSecondaryBrush"), HorizontalAlignment = HorizontalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = Math.Max(10, Number(m["width"]) * scale - 20) });
            var tile = new Border { Width = Number(m["width"]) * scale, Height = Number(m["height"]) * scale, MinHeight = 44, CornerRadius = new CornerRadius(7), BorderThickness = new Thickness(local ? 2 : 1), BorderBrush = Brush(local ? "AccentFillColorDefaultBrush" : "CardStrokeColorDefaultBrush"), Background = Brush("CardBackgroundFillColorDefaultBrush"), Child = stack };
            Canvas.SetLeft(tile, ox + (Number(m["x"]) - left) * scale); Canvas.SetTop(tile, oy + (Number(m["y"]) - top) * scale);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(tile, $"{Text(m["label"])} screen. Drag to arrange.");
            ToolTipService.SetToolTip(tile, Text(m["label"]));
            AttachDrag(tile, m);
            Arrangement.Children.Add(tile);
        }
    }
    private static Brush Brush(string key) => (Brush)Application.Current.Resources[key];
    private void AttachDrag(Border tile, JsonObject monitor)
    {
        Point start = default; double x = 0, y = 0; int originalX = 0, originalY = 0;
        tile.PointerPressed += (_, e) =>
        {
            if (applying || !e.GetCurrentPoint(tile).Properties.IsLeftButtonPressed) return;
            start = e.GetCurrentPoint(Arrangement).Position; x = Canvas.GetLeft(tile); y = Canvas.GetTop(tile);
            originalX = Number(monitor["x"]); originalY = Number(monitor["y"]); dragging = tile.CapturePointer(e.Pointer); e.Handled = true;
        };
        tile.PointerMoved += (_, e) => { if (!dragging || tile.PointerCaptures.Count == 0) return; var p = e.GetCurrentPoint(Arrangement).Position; Canvas.SetLeft(tile, x + p.X - start.X); Canvas.SetTop(tile, y + p.Y - start.Y); };
        tile.PointerReleased += async (_, e) =>
        {
            if (!dragging || tile.PointerCaptures.Count == 0 || state?["layout"] is not JsonObject layout) return;
            var copy = layout.DeepClone().AsObject(); var array = copy["monitors"]!.AsArray();
            var moving = array.First(m => Text(m?["id"]) == Text(monitor["id"]))!.AsObject();
            int newX = originalX + (int)Math.Round((Canvas.GetLeft(tile) - x) / scale), newY = originalY + (int)Math.Round((Canvas.GetTop(tile) - y) / scale);
            // Snap the nearest pair of parallel edges, preserving free movement along the other axis.
            double best = 28 / scale; int snappedX = newX, snappedY = newY;
            foreach (var other in array.Where(n => Text(n?["id"]) != Text(moving["id"]) && (n?["display"]?["active"] is null || Flag(n["display"]!["active"]))))
            {
                int ax = Number(other?["x"]), ay = Number(other?["y"]), aw = Number(other?["width"]), ah = Number(other?["height"]), mw = Number(moving["width"]), mh = Number(moving["height"]);
                if (newY < ay + ah && newY + mh > ay) foreach (int target in new[] { ax - mw, ax + aw }) if (Math.Abs(newX - target) < best) { best = Math.Abs(newX - target); snappedX = target; snappedY = newY; }
                if (newX < ax + aw && newX + mw > ax) foreach (int target in new[] { ay - mh, ay + ah }) if (Math.Abs(newY - target) < best) { best = Math.Abs(newY - target); snappedX = newX; snappedY = target; }
            }
            moving["x"] = snappedX; moving["y"] = snappedY;
            dragging = false; tile.ReleasePointerCaptures();
            await SendAsync(new { command = "arrange", layout = copy, version = state["layout_version"]!.GetValue<ulong>() }); previousView = ""; await RefreshAsync();
        };
        tile.PointerCaptureLost += (_, _) => { if (dragging) { dragging = false; DrawArrangement(); } };
    }
    private void DrawPeers()
    {
        var expanded = PeersPanel.Children.OfType<SettingsExpander>().Where(card => card.IsExpanded).Select(card => card.Tag as string).ToHashSet();
        PeersPanel.Children.Clear();
        if (state?["peers"] is not JsonArray peers || peers.Count == 0) { PeersPanel.Children.Add(new SettingsCard { Header = "No computers added", Description = "Find a nearby computer or add one by its Tailscale name or address.", HeaderIcon = new FontIcon { Glyph = "\uE7F4" } }); return; }
        foreach (var peer in peers)
        {
            string name = Text(peer?["name"]); bool connected = Flag(peer?["connected"]);
            var card = new SettingsExpander
            {
                Header = name, Tag = name, IsExpanded = expanded.Contains(name),
                Description = connected ? "Connected · Mark " + Text(peer?["mark"]) : Text(peer?["status"], "Connecting…"),
                HeaderIcon = new FontIcon { Glyph = "\uE7F4" }
            };
            var control = new Button { Content = "Control", IsEnabled = connected };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(control, "Control " + name);
            control.Click += async (_, _) => await SendAsync(new { command = "activate", peer = name });
            card.Content = control;
            var mode = new ComboBox { MinWidth = 170, ItemsSource = new[] { "Standard keys", "PC key positions", "Mac shortcuts" }, SelectedIndex = Text(peer?["keyboard"]) switch { "pc_positions" => 1, "mac" => 2, _ => 0 } };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(mode, "Keyboard mode for " + name);
            mode.SelectionChanged += async (_, _) => { if (!applying && mode.SelectedIndex >= 0) await SendAsync(new { command = "keyboard", peer = name, mode = new[] { "standard", "pc_positions", "mac" }[mode.SelectedIndex] }); };
            card.Items.Add(new SettingsCard { Header = "Keyboard mode", Description = "How keys from this computer work here.", Content = mode });
            var scroll = new ToggleSwitch { IsOn = Flag(peer?["reverse_scroll"]) };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(scroll, "Reverse scrolling for " + name);
            scroll.Toggled += async (_, _) => { if (!applying) await SendAsync(new { command = "reverse_scroll", peer = name, enabled = scroll.IsOn }); };
            card.Items.Add(new SettingsCard { Header = "Reverse scrolling", Content = scroll });
            var forget = new Button { Content = "Remove" };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(forget, "Remove " + name);
            forget.Click += async (_, _) =>
            {
                var dialog = new ContentDialog { XamlRoot = Root.XamlRoot, Title = "Remove " + name + "?", Content = "You’ll need to add this computer again to share input.", PrimaryButtonText = "Remove", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Close };
                if (await dialog.ShowAsync() == ContentDialogResult.Primary) await SendAsync(new { command = "forget", peer = name });
            };
            card.Items.Add(new SettingsCard { Header = "Remove computer", Content = forget });
            PeersPanel.Children.Add(card);
        }
    }
    private void DrawNearby()
    {
        NearbyPanel.Children.Clear();
        bool found = state?["nearby"] is JsonArray { Count: > 0 };
        NearbySection.Visibility = found || state?["peers"] is not JsonArray { Count: > 0 } ? Visibility.Visible : Visibility.Collapsed;
        if (state?["nearby"] is not JsonArray nearby || nearby.Count == 0) { NearbyPanel.Children.Add(new TextBlock { Text = "Looking for computers running zflow…", TextWrapping = TextWrapping.Wrap, Foreground = Brush("TextFillColorSecondaryBrush") }); return; }
        foreach (var peer in nearby)
        {
            string details = Text(peer?["state"]) switch {
                "different_version" => "Different zflow version. Update both computers.",
                "identifying" => "Waiting for this computer to identify itself…",
                "duplicate_name" => "Same name as another computer · compare mark " + Text(peer?["mark"]),
                _ => Text(peer?["os"]) + " · Mark " + Text(peer?["mark"])
            };
            string id = Text(peer?["id"]); var add = new Button { Content = "Add computer", IsEnabled = Text(peer?["state"]) is "ready" or "duplicate_name" };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(add, "Add " + Text(peer?["name"]));
            add.Click += async (_, _) => await SendAsync(new { command = "trust", key = id });
            NearbyPanel.Children.Add(new SettingsCard { Header = Text(peer?["name"]), Description = details, HeaderIcon = new FontIcon { Glyph = "\uE7F4" }, Content = add });
        }
    }
    private async void AddAddress_Click(object sender, RoutedEventArgs e)
    {
        var address = new TextBox { PlaceholderText = "macbook or 100.x.x.x", Header = "Computer name or IP address" };
        var content = new StackPanel { Spacing = 16 };
        content.Children.Add(new TextBlock { Text = "Enter a local or Tailscale address. Make sure zflow is running on the other computer.", TextWrapping = TextWrapping.Wrap });
        content.Children.Add(address);
        var dialog = new ContentDialog { XamlRoot = Root.XamlRoot, Title = "Add computer", Content = content, PrimaryButtonText = "Find computer", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Primary, IsPrimaryButtonEnabled = false };
        address.TextChanged += (_, _) => dialog.IsPrimaryButtonEnabled = !string.IsNullOrWhiteSpace(address.Text);
        if (await dialog.ShowAsync() == ContentDialogResult.Primary && !string.IsNullOrWhiteSpace(address.Text)) await SendAsync(new { command = "nearby", address = address.Text.Trim() });
    }
    private async void Sharing_Toggled(object sender, RoutedEventArgs e) { if (!applying && state is not null) await SendAsync(new { command = "sharing", enabled = SharingSwitch.IsOn }); }
    private async void Clipboard_Toggled(object sender, RoutedEventArgs e) { if (!applying && state is not null) await SendAsync(new { command = "clipboard", enabled = ClipboardSwitch.IsOn }); }
    private async void PauseEdges_Toggled(object sender, RoutedEventArgs e) { if (!applying && state is not null) await SendAsync(new { command = "pause_at_edges", enabled = PauseEdgesSwitch.IsOn }); }
    private async void Local_Click(object sender, RoutedEventArgs e) => await SendAsync(new { command = "local" });
    private void Navigation_SelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        if (args.SelectedItem is not NavigationViewItem item || ComputersPage is null) return;
        string page = item.Tag as string ?? "computers";
        ComputersPage.Visibility = page == "computers" ? Visibility.Visible : Visibility.Collapsed;
        SettingsPage.Visibility = page == "settings" ? Visibility.Visible : Visibility.Collapsed;
        AboutPage.Visibility = page == "about" ? Visibility.Visible : Visibility.Collapsed;
        PageTitle.Text = item.Content.ToString();
        PageScroll.ChangeView(null, 0, null, true);
        if (page == "computers") DrawArrangement();
    }
    private async void OpenLink_Click(object sender, RoutedEventArgs e)
    {
        try { if (sender is FrameworkElement { Tag: string url }) await Windows.System.Launcher.LaunchUriAsync(new Uri(url)); }
        catch (Exception error) { ShowError(error); }
    }
    private void Arrangement_SizeChanged(object sender, SizeChangedEventArgs e) => DrawArrangement();
    private void Startup_Toggled(object sender, RoutedEventArgs e)
    {
        if (applying) return;
        try { using var key = Registry.CurrentUser.CreateSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run"); if (StartupSwitch.IsOn) key.SetValue("zflow", $"\"{Environment.ProcessPath}\" --background"); else key.DeleteValue("zflow", false); }
        catch (Exception error) { ShowError(error); }
    }
    private void OpenConfig_Click(object sender, RoutedEventArgs e) { try { var start = new ProcessStartInfo("notepad.exe") { UseShellExecute = false }; start.ArgumentList.Add(EngineClient.ConfigPath); Process.Start(start); } catch (Exception error) { ShowError(error); } }
    private async void Restart_Click(object sender, RoutedEventArgs e) { try { await engine.SendAsync(new { command = "quit" }, 1500); } catch { } await Task.Delay(500); await StartAsync(); }
    private async void Quit_Click(object sender, RoutedEventArgs e) => await QuitAsync();
    private async Task QuitAsync() { if (quitting) return; quitting = true; timer.Stop(); try { await engine.SendAsync(new { command = "quit" }, 2000); } catch { } native.Dispose(); Close(); Application.Current.Exit(); }
    private async Task CapturePreviewAsync(string path)
    {
        try
        {
            // Render the real WinUI visual tree for visual QA on machines whose
            // graphics driver does not expose the composition surface to capture.
            await Task.Delay(500);
            var bitmap = new Microsoft.UI.Xaml.Media.Imaging.RenderTargetBitmap();
            await bitmap.RenderAsync(Root);
            var pixels = (await bitmap.GetPixelsAsync()).ToArray();
            using var stream = File.Create(Path.GetFullPath(path)).AsRandomAccessStream();
            var encoder = await Windows.Graphics.Imaging.BitmapEncoder.CreateAsync(Windows.Graphics.Imaging.BitmapEncoder.PngEncoderId, stream);
            encoder.SetPixelData(Windows.Graphics.Imaging.BitmapPixelFormat.Bgra8, Windows.Graphics.Imaging.BitmapAlphaMode.Premultiplied, (uint)bitmap.PixelWidth, (uint)bitmap.PixelHeight, 96, 96, pixels);
            await encoder.FlushAsync();
        }
        catch (Exception error) { ShowError(error); }
    }
}
