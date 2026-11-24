namespace MwbBridgeCompanion;

static class Program
{
    [STAThread] // OLE drag and drop needs a single-threaded apartment
    static void Main()
    {
        using var single = new Mutex(true, @"Local\MwbBridgeCompanion", out var first);
        if (!first) return;

        ApplicationConfiguration.Initialize();
        Log.Write("Starting.");
        var config = Config.Load();
        if (config.SecurityKey == null)
        {
            MessageBox.Show($"No security key found: Mouse Without Borders doesn't seem to be set up in PowerToys.\n\nSet it up there, or put one in {Config.FilePath} as\n{{\"security_key\": \"...\"}}",
                "MWB Bridge Companion", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            return;
        }
        IncomingDrop.PruneOld();
        Application.Run(new TrayContext(config));
    }
}

sealed class TrayContext : ApplicationContext
{
    readonly NotifyIcon _tray;
    bool _lightTaskbar;
    readonly ToolStripMenuItem _statusItem = new("Waiting for the bridge") { Enabled = false };
    readonly Control _ui = new();
    ClipboardWatcher? _clipboard;

    public TrayContext(Config config)
    {
        _ = _ui.Handle; // the UI thread's marshalling target
        var menu = new ContextMenuStrip();
        menu.Items.Add(_statusItem);
        menu.Items.Add(new ToolStripSeparator());
        menu.Items.Add("Open log", null, (_, _) => System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(Log.FilePath) { UseShellExecute = true }));
        menu.Items.Add("Quit", null, (_, _) => { _tray!.Visible = false; ExitThread(); });
        _tray = new NotifyIcon { Icon = TrayIcon(_lightTaskbar = LightTaskbar()), Text = "MWB Bridge Companion", ContextMenuStrip = menu, Visible = true };
        Microsoft.Win32.SystemEvents.UserPreferenceChanged += OnPreferenceChanged;

        var coordinator = new Coordinator(_ui);
        new Server(config, coordinator, SetStatus).Start();
        _clipboard = new ClipboardWatcher(coordinator.OnClipboardChanged);

        if (Config.MwbTransferFileOn())
        {
            Log.Write("PowerToys' \"Transfer file\" is on: it catches the same drags, turn it off in Mouse Without Borders' settings.");
            _tray.ShowBalloonTip(10000, "MWB Bridge Companion",
                "Turn off \"Transfer file\" in PowerToys' Mouse Without Borders settings: it handles the same drags as this app.", ToolTipIcon.Warning);
        }
    }

    /// The Mac bridge's arrow, black on a light taskbar and white on a dark
    /// one (macOS recolors its menu bar icon the same way), at the tray's size.
    static Icon TrayIcon(bool light)
    {
        using var stream = typeof(TrayContext).Assembly.GetManifestResourceStream(light ? "tray-light.ico" : "tray-dark.ico");
        return stream == null ? SystemIcons.Application : new Icon(stream, SystemInformation.SmallIconSize);
    }

    /// Whether the taskbar uses the light theme. Missing means an older
    /// Windows, whose taskbar is always dark.
    static bool LightTaskbar()
    {
        using var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        return key?.GetValue("SystemUsesLightTheme") is int light && light != 0;
    }

    /// Follows a switch between light and dark mode.
    void OnPreferenceChanged(object? sender, Microsoft.Win32.UserPreferenceChangedEventArgs e)
    {
        if (e.Category != Microsoft.Win32.UserPreferenceCategory.General) return;
        _ui.BeginInvoke(() =>
        {
            var light = LightTaskbar();
            if (light == _lightTaskbar) return;
            _lightTaskbar = light;
            var old = _tray.Icon;
            _tray.Icon = TrayIcon(light);
            old?.Dispose();
        });
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing) Microsoft.Win32.SystemEvents.UserPreferenceChanged -= OnPreferenceChanged;
        base.Dispose(disposing);
    }

    void SetStatus(string? bridge)
    {
        _ui.BeginInvoke(() =>
        {
            _statusItem.Text = bridge == null ? "Waiting for the bridge" : $"Connected to {bridge}";
            _tray.Text = bridge == null ? "MWB Bridge Companion: waiting" : $"MWB Bridge Companion: {bridge}";
        });
    }
}

/// Calls back on the UI thread whenever the clipboard changes.
sealed class ClipboardWatcher : NativeWindow
{
    readonly Action _changed;

    public ClipboardWatcher(Action changed)
    {
        _changed = changed;
        CreateHandle(new CreateParams());
        if (!Native.AddClipboardFormatListener(Handle))
        {
            Log.Write($"Couldn't watch the clipboard: {System.Runtime.InteropServices.Marshal.GetLastWin32Error()}");
        }
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == Native.WM_CLIPBOARDUPDATE) _changed();
        base.WndProc(ref m);
    }
}
