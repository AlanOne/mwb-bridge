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
    // Shown only while a transfer runs.
    readonly ToolStripMenuItem _transferItem = new("") { Enabled = false, Available = false };
    readonly ToolStripMenuItem _cancelItem = new("Cancel transfer") { Available = false };
    readonly System.Windows.Forms.Timer _progressTimer = new() { Interval = 500 };
    // The percentage the tray icon shows while a transfer runs (-1: the
    // usual icon is up).
    int _iconPercent = -1;
    string? _bridge;
    readonly Control _ui = new();
    ClipboardWatcher? _clipboard;

    public TrayContext(Config config)
    {
        _ = _ui.Handle; // the UI thread's marshalling target
        var menu = new ContextMenuStrip();
        menu.Items.Add(_statusItem);
        menu.Items.Add(_transferItem);
        menu.Items.Add(_cancelItem);
        menu.Items.Add(new ToolStripSeparator());
        menu.Items.Add("Open log", null, (_, _) => System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(Log.FilePath) { UseShellExecute = true }));
        menu.Items.Add("Quit", null, (_, _) => { _tray!.Visible = false; ExitThread(); });
        _tray = new NotifyIcon { Icon = TrayIcon(_lightTaskbar = LightTaskbar()), Text = "MWB Bridge Companion", ContextMenuStrip = menu, Visible = true };
        Microsoft.Win32.SystemEvents.UserPreferenceChanged += OnPreferenceChanged;

        var coordinator = new Coordinator(_ui);
        var server = new Server(config, coordinator, SetStatus, Warn);
        server.Start();
        _cancelItem.Click += (_, _) => server.CancelAll();
        _progressTimer.Tick += (_, _) => ShowProgress();
        _progressTimer.Start();
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
            _iconPercent = -1; // a percentage icon is redrawn next tick
        });
    }

    void SetTrayIcon(Icon icon, int percent)
    {
        var old = _tray.Icon;
        _tray.Icon = icon;
        old?.Dispose();
        _iconPercent = percent;
    }

    /// `percent` drawn at the tray's icon size, with a bar along the bottom
    /// filled that far: black on a light taskbar, white on a dark one.
    static Icon PercentIcon(int percent, bool light)
    {
        var size = SystemInformation.SmallIconSize;
        using var bitmap = new Bitmap(size.Width, size.Height, System.Drawing.Imaging.PixelFormat.Format32bppArgb);
        using (var g = Graphics.FromImage(bitmap))
        {
            g.Clear(Color.Transparent);
            g.TextRenderingHint = System.Drawing.Text.TextRenderingHint.AntiAliasGridFit;
            var ink = light ? Color.Black : Color.White;
            var bar = Math.Max(2, size.Height / 8);
            var text = percent.ToString();
            using var font = new Font("Segoe UI", size.Height * (text.Length > 1 ? 0.6f : 0.7f), FontStyle.Bold, GraphicsUnit.Pixel);
            using var brush = new SolidBrush(ink);
            using var format = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
            g.DrawString(text, font, brush, new RectangleF(-1, -1, size.Width + 2, size.Height - bar), format);
            using var track = new SolidBrush(Color.FromArgb(80, ink));
            g.FillRectangle(track, 0, size.Height - bar, size.Width, bar);
            g.FillRectangle(brush, 0, size.Height - bar, size.Width * percent / 100f, bar);
        }
        var handle = bitmap.GetHicon();
        try
        {
            using var borrowed = Icon.FromHandle(handle);
            return (Icon)borrowed.Clone(); // owns a copy of the handle
        }
        finally
        {
            Native.DestroyIcon(handle);
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing) Microsoft.Win32.SystemEvents.UserPreferenceChanged -= OnPreferenceChanged;
        base.Dispose(disposing);
    }

    /// A setup problem worth a balloon, not just a log line.
    void Warn(string message)
    {
        _ui.BeginInvoke(() => _tray.ShowBalloonTip(15000, "MWB Bridge Companion", message, ToolTipIcon.Warning));
    }

    void SetStatus(string? bridge)
    {
        _ui.BeginInvoke(() =>
        {
            _bridge = bridge;
            _statusItem.Text = bridge == null ? "Waiting for the bridge" : $"Connected to {bridge}";
            _tray.Text = TrayText();
        });
    }

    string TrayText() => _bridge == null ? "MWB Bridge Companion: waiting" : $"MWB Bridge Companion: {_bridge}";

    /// The running transfers' line and "Cancel transfer" in the menu, their
    /// percentage as the tray icon (as the Mac shows it next to its menu bar
    /// icon), and in the tooltip.
    void ShowProgress()
    {
        var progress = Transfers.ProgressLine(_bridge ?? "the bridge");
        var shown = progress != null;
        if (progress is var (line, pct))
        {
            _transferItem.Text = line;
            var tip = $"MWB Bridge Companion: {(Transfers.Snapshot().All(t => t.Sending) ? "sending" : "receiving")} {pct}%";
            if (_tray.Text != tip) _tray.Text = tip;
            if (pct != _iconPercent) SetTrayIcon(PercentIcon(pct, _lightTaskbar), pct);
        }
        else if (_transferItem.Available || _iconPercent >= 0)
        {
            _tray.Text = TrayText();
            if (_iconPercent >= 0) SetTrayIcon(TrayIcon(_lightTaskbar), -1);
        }
        if (_transferItem.Available != shown)
        {
            _transferItem.Available = shown;
            _cancelItem.Available = shown;
        }
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
