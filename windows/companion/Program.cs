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
        if (config == null)
        {
            MessageBox.Show($"No security key configured.\n\nCreate {Config.FilePath} with\n{{\"security_key\": \"<the Mouse Without Borders key>\"}}",
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
    readonly ToolStripMenuItem _statusItem = new("Waiting for the bridge") { Enabled = false };
    readonly Control _ui = new();

    public TrayContext(Config config)
    {
        _ = _ui.Handle; // the UI thread's marshalling target
        var menu = new ContextMenuStrip();
        menu.Items.Add(_statusItem);
        menu.Items.Add(new ToolStripSeparator());
        menu.Items.Add("Open log", null, (_, _) => System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(Log.FilePath) { UseShellExecute = true }));
        menu.Items.Add("Quit", null, (_, _) => { _tray!.Visible = false; ExitThread(); });
        _tray = new NotifyIcon { Icon = SystemIcons.Application, Text = "MWB Bridge Companion", ContextMenuStrip = menu, Visible = true };

        var coordinator = new Coordinator(_ui);
        new Server(config, coordinator, SetStatus).Start();

        if (Config.MwbTransferFileOn())
        {
            Log.Write("PowerToys' \"Transfer file\" is on: it catches the same drags, turn it off in Mouse Without Borders' settings.");
            _tray.ShowBalloonTip(10000, "MWB Bridge Companion",
                "Turn off \"Transfer file\" in PowerToys' Mouse Without Borders settings: it handles the same drags as this app.", ToolTipIcon.Warning);
        }
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
