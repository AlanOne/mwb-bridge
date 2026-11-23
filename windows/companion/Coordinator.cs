using System.Collections.Concurrent;
using System.Diagnostics;

namespace MwbBridgeCompanion;

/// Everything drag-related, on the UI thread. The server calls in through
/// `Post`.
sealed class Coordinator
{
    readonly Control _ui;
    readonly CatcherWindow _catcher = new();
    readonly SourceWindow _source = new();
    readonly System.Windows.Forms.Timer _raise = new() { Interval = 100 };

    public Func<uint, object, bool>? SendToBridge { get; set; }

    // Windows -> Mac drags offered to the bridge, by id: the files it fetches.
    readonly ConcurrentDictionary<ulong, string[]> _toMac = new();
    // Mac -> Windows drags, by id.
    readonly ConcurrentDictionary<ulong, IncomingDrop> _fromMac = new();
    ulong _nextId = (ulong)DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    // Windows -> Mac state.
    bool _catching;      // the catcher is up for a drag that left Windows
    bool _caughtDrag;    // a drag is hovering over it

    // Mac -> Windows state.
    IncomingDrop? _pendingSource; // waiting for the simulated press to land

    public Coordinator(Control ui)
    {
        _ui = ui;
        _catcher.FilesCaught += OnFilesCaught;
        _catcher.DragEntered += keyState =>
        {
            if (!_caughtDrag) Log.Write($"[to-mac] A drag entered the catcher (key state {keyState}: left button {((keyState & 1) != 0 ? "down" : "up")} as the drag sees it).");
            _caughtDrag = true;
        };
        _catcher.Dropped += () => Log.Write("[to-mac] The leftover Windows drag ended on the catcher.");
        // Mouse Without Borders keeps its own small topmost window at the
        // parked cursor; stay above it so the drag reaches the catcher.
        _raise.Tick += (_, _) => { if (_catcher.Visible) Native.RaiseTopmost(_catcher.Handle, SystemInformation.VirtualScreen); };
        _source.Pressed += OnSourcePressed;
        _ = _catcher.Handle;
        _ = _source.Handle;
    }

    public void Post(Action action)
    {
        if (_ui.IsHandleCreated) _ui.BeginInvoke(action);
    }

    // ---- Windows -> Mac ------------------------------------------------

    /// The bridge's machine became the active one. If something was being
    /// dragged across, catch it. Windows' own idea of the left button
    /// can't decide that: live, it reported the button up during every
    /// drag that crossed, so the catcher goes up on every switch and simply
    /// stands down again if no drag arrives. While the other machine has
    /// control the Windows cursor is parked and hidden, so it's unseen.
    public void OnActive()
    {
        if (_catching) return;
        Log.Write($"[to-mac] The bridge's machine is active (left button {(Native.LeftButtonDown() ? "down" : "up")} as Windows reports it), watching for a drag.");
        _catching = true;
        _caughtDrag = false;
        _catcher.ShowOver(SystemInformation.VirtualScreen);
        _raise.Start();
        // Not every held-button crossing is a drag of something (moving a
        // window to the edge): give up if no drag shows up.
        var timer = new System.Windows.Forms.Timer { Interval = 1500 };
        timer.Tick += (_, _) =>
        {
            timer.Dispose();
            if (_catching && !_caughtDrag)
            {
                HideCatcher();
            }
        };
        timer.Start();
    }

    void OnFilesCaught(string[] dropped)
    {
        if (!_catching) return;
        var files = dropped.Where(File.Exists).ToArray();
        foreach (var skipped in dropped.Where(p => !File.Exists(p)))
        {
            Log.Write($"[to-mac] Skipping {skipped}: folders aren't supported, zip it first.");
        }
        if (files.Length == 0) return;
        var id = Interlocked.Increment(ref _nextId);
        _toMac[id] = files;
        var offer = new OfferMsg(id, files.Select(f => new FileInfoMsg(Path.GetFileName(f), (ulong)new FileInfo(f).Length)).ToList());
        Log.Write($"[to-mac] Offering {files.Length} file(s) to the bridge as drag {id}: {string.Join(", ", offer.Files.Select(f => f.Name))}");
        SendToBridge?.Invoke(Frames.OfferToMac, offer);
    }

    /// Control came back to Windows.
    public void OnInactive(bool buttonHeld)
    {
        if (!_catching) return;
        if (buttonHeld)
        {
            // Dragged back to Windows: let the drag carry on here.
            Log.Write("[to-mac] The drag came back to Windows, handing it back.");
            HideCatcher();
            return;
        }
        if (!_caughtDrag)
        {
            HideCatcher();
            return;
        }
        // Let go on the other machine. The release happened over there, so
        // Windows' drag is still going: release it here over the catcher,
        // which ends it harmlessly.
        _catcher.ShowOver(SystemInformation.VirtualScreen);
        var timer = new System.Windows.Forms.Timer { Interval = 50 };
        timer.Tick += (_, _) =>
        {
            timer.Dispose();
            Log.Write("[to-mac] Released on the other machine, ending the leftover Windows drag.");
            Native.LeftButton(down: false);
            var hide = new System.Windows.Forms.Timer { Interval = 300 };
            hide.Tick += (_, _) => { hide.Dispose(); HideCatcher(); };
            hide.Start();
        };
        timer.Start();
    }

    void HideCatcher()
    {
        _raise.Stop();
        _catcher.Hide();
        _catching = _caughtDrag = false;
    }

    /// The bridge fetching a drag's files.
    public string[]? FilesToMac(ulong id) => _toMac.TryGetValue(id, out var f) ? f : null;

    // ---- Mac -> Windows ------------------------------------------------

    /// Files dragged on the Mac came to Windows: start a Windows drag of
    /// them under the cursor while the bytes arrive.
    // ---- File copy and paste ----------------------------------------

    string[] _lastSetOnClipboard = Array.Empty<string>();
    string[] _lastOffered = Array.Empty<string>();
    DateTime _lastOfferedAt;

    /// Files copied on the Mac: put them on Windows' clipboard once they've
    /// all arrived, as if copied in Explorer.
    public IncomingDrop RegisterCopy(OfferMsg offer)
    {
        var drop = IncomingDrop.Create(offer, IncomingDrop.CopiesRoot);
        _fromMac[offer.Id] = drop;
        drop.Completed = () => Post(() =>
        {
            try
            {
                var list = new System.Collections.Specialized.StringCollection();
                list.AddRange(drop.Paths);
                Clipboard.SetFileDropList(list);
                _lastSetOnClipboard = drop.Paths;
                Log.Write($"[copy] {drop.Paths.Length} file(s) copied on the Mac are on the clipboard.");
            }
            catch (Exception e)
            {
                Log.Write($"[copy] Couldn't put the files copied on the Mac on the clipboard: {e.Message}");
            }
        });
        return drop;
    }

    /// The Windows clipboard changed: if it's a file copy made here, offer
    /// the files to the bridge.
    public void OnClipboardChanged()
    {
        string[] files;
        try
        {
            if (!Clipboard.ContainsFileDropList()) return;
            files = Clipboard.GetFileDropList().Cast<string>().ToArray();
        }
        catch (Exception e)
        {
            Log.Write($"[copy] Couldn't read the clipboard: {e.Message}");
            return;
        }
        // Our own paste of the Mac's files, or Explorer announcing the same
        // copy twice in a row.
        if (files.SequenceEqual(_lastSetOnClipboard)) return;
        if (files.SequenceEqual(_lastOffered) && DateTime.Now - _lastOfferedAt < TimeSpan.FromSeconds(2)) return;
        foreach (var skipped in files.Where(f => !File.Exists(f)))
        {
            Log.Write($"[copy] Skipping {skipped}: folders aren't supported, zip it first.");
        }
        var sendable = files.Where(File.Exists).ToArray();
        if (sendable.Length == 0) return;
        _lastOffered = files;
        _lastOfferedAt = DateTime.Now;
        var id = Interlocked.Increment(ref _nextId);
        _toMac[id] = sendable;
        var offer = new OfferMsg(id, sendable.Select(f => new FileInfoMsg(Path.GetFileName(f), (ulong)new FileInfo(f).Length)).ToList());
        var sent = SendToBridge?.Invoke(Frames.ClipboardToMac, offer) == true;
        Log.Write($"[copy] {sendable.Length} file(s) copied here {(sent ? "offered to the bridge" : "not sent: no bridge connected")} ({id}): {string.Join(", ", offer.Files.Select(f => f.Name))}");
    }

    public IncomingDrop Register(OfferMsg offer)
    {
        var drop = IncomingDrop.Create(offer);
        _fromMac[offer.Id] = drop;
        return drop;
    }

    public void OnOfferToWindows(IncomingDrop drop)
    {
        Log.Write($"[to-windows] Drag {drop.Id}: {drop.Paths.Length} file(s) from the bridge, starting a drag here.");
        Log.Write($"[to-windows] Cursor at {Cursor.Position}, left button {(Native.LeftButtonDown() ? "down" : "up")} as Windows sees it.");
        if (!Native.LeftButtonDown())
        {
            // Windows doesn't know about the button held on the other
            // machine; this press, on the source window, gives it one.
            _pendingSource = drop;
            var at = Cursor.Position;
            _source.ShowOver(new Rectangle(at.X - 32, at.Y - 32, 64, 64));
            var press = new System.Windows.Forms.Timer { Interval = 40 };
            press.Tick += (_, _) =>
            {
                press.Dispose();
                // Again right before pressing: other topmost windows (Mouse
                // Without Borders keeps one at the cursor) may have come up.
                var now = Cursor.Position;
                _source.ShowOver(new Rectangle(now.X - 32, now.Y - 32, 64, 64));
                Log.Write($"[to-windows] Pressing at {now} on {Native.DescribeWindowAt(now)} (the source window is 0x{_source.Handle.ToInt64():x}).");
                Native.LeftButton(down: true);
            };
            press.Start();
            // If the press didn't land on the window, release it again and
            // save the files where PowerToys would have.
            var check = new System.Windows.Forms.Timer { Interval = 600 };
            check.Tick += (_, _) =>
            {
                check.Dispose();
                if (_pendingSource == drop)
                {
                    _pendingSource = null;
                    _source.Hide();
                    Native.LeftButton(down: false);
                    Log.Write($"[to-windows] Drag {drop.Id} didn't start, saving its files to the desktop instead.");
                    drop.SaveToDesktopWhenComplete();
                }
            };
            check.Start();
        }
        else
        {
            Log.Write($"[to-windows] Drag {drop.Id}: the button is already down here, saving to the desktop instead.");
            drop.SaveToDesktopWhenComplete();
        }
    }

    void OnSourcePressed()
    {
        Log.Write("[to-windows] The press landed on the source window.");
        var drop = _pendingSource;
        _pendingSource = null;
        if (drop == null) return;
        // Out of the way of the drop once the drag is running.
        BeginInvokeLater(() => _source.Bounds = new Rectangle(-32000, -32000, 1, 1));
        var data = new DataObject(DataFormats.FileDrop, drop.Paths);
        var released = false;
        void Query(object? sender, QueryContinueDragEventArgs e)
        {
            if (e.EscapePressed || drop.Cancelled)
            {
                e.Action = DragAction.Cancel;
            }
            else if ((e.KeyState & 1) == 0)
            {
                if (!released)
                {
                    released = true;
                    if (!drop.IsComplete) Log.Write($"[to-windows] Drag {drop.Id} let go, waiting for its files to finish arriving.");
                }
                // Hold the drop until every file has fully arrived.
                e.Action = drop.IsComplete ? DragAction.Drop : drop.Failed ? DragAction.Cancel : DragAction.Continue;
            }
        }
        _source.QueryContinueDrag += Query;
        var effect = _source.DoDragDrop(data, DragDropEffects.Copy);
        _source.QueryContinueDrag -= Query;
        _source.Hide();
        Log.Write($"[to-windows] Drag {drop.Id} ended: {(effect == DragDropEffects.None ? "not dropped anywhere that takes it (or cancelled)" : "dropped into an app")}.");
    }

    void BeginInvokeLater(Action action)
    {
        var t = new System.Windows.Forms.Timer { Interval = 100 };
        t.Tick += (_, _) => { t.Dispose(); action(); };
        t.Start();
    }

    public void OnCancel(ulong id)
    {
        if (_fromMac.TryGetValue(id, out var drop)) drop.Cancelled = true;
    }

    public IncomingDrop? FromMac(ulong id) => _fromMac.TryGetValue(id, out var d) ? d : null;

    public void OnBridgeDisconnected()
    {
        if (_catching) HideCatcher();
        foreach (var drop in _fromMac.Values) drop.Cancelled = true;
    }
}

/// A Mac -> Windows drag's files, written as they arrive.
sealed class IncomingDrop
{
    public ulong Id { get; }
    public string[] Paths { get; }
    readonly bool[] _done;
    public volatile bool Cancelled;
    public volatile bool Failed;
    bool _saveToDesktop;

    public bool IsComplete { get { lock (_done) return _done.All(d => d); } }

    IncomingDrop(ulong id, string[] paths)
    {
        Id = id;
        Paths = paths;
        _done = new bool[paths.Length];
    }

    public static string Root { get; } = Path.Combine(Path.GetTempPath(), "MwbBridgeCompanion", "drops");
    public static string CopiesRoot { get; } = Path.Combine(Path.GetTempPath(), "MwbBridgeCompanion", "copies");

    /// Runs (on a network thread) once every file has arrived.
    public Action? Completed { get; set; }

    public static IncomingDrop Create(OfferMsg offer, string? root = null)
    {
        var dir = Path.Combine(root ?? Root, offer.Id.ToString());
        Directory.CreateDirectory(dir);
        var paths = offer.Files.Select(f => Path.Combine(dir, SafeName(f.Name))).ToArray();
        // Empty placeholders, so the drag can start before the bytes arrive.
        foreach (var p in paths) File.WriteAllBytes(p, Array.Empty<byte>());
        return new IncomingDrop(offer.Id, paths);
    }

    static string SafeName(string name)
    {
        var bad = Path.GetInvalidFileNameChars();
        var clean = new string(name.Select(c => bad.Contains(c) ? '_' : c).ToArray()).Trim();
        return clean is "" or "." or ".." ? "file" : clean;
    }

    public void FileDone(int index)
    {
        bool all;
        lock (_done)
        {
            _done[index] = true;
            all = _done.All(d => d);
        }
        if (all)
        {
            Log.Write($"[to-windows] {Id}: all {Paths.Length} file(s) arrived.");
            Completed?.Invoke();
            if (_saveToDesktop) SaveToDesktop();
        }
    }

    public void SaveToDesktopWhenComplete()
    {
        _saveToDesktop = true;
        if (IsComplete) SaveToDesktop();
    }

    void SaveToDesktop()
    {
        var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Desktop), "MouseWithoutBorders");
        Directory.CreateDirectory(dir);
        foreach (var p in Paths) File.Copy(p, Path.Combine(dir, Path.GetFileName(p)), overwrite: true);
        Log.Write($"[to-windows] Drag {Id}: saved to {dir}.");
        Process.Start(new ProcessStartInfo(dir) { UseShellExecute = true });
    }

    /// Drop folders older than a day: apps may read a dropped file a while
    /// after the drop, so they aren't deleted right away.
    public static void PruneOld()
    {
        try
        {
            foreach (var d in new[] { Root, CopiesRoot }.Where(Directory.Exists).SelectMany(Directory.GetDirectories))
            {
                if (DateTime.Now - Directory.GetLastWriteTime(d) > TimeSpan.FromDays(1)) Directory.Delete(d, recursive: true);
            }
        }
        catch (Exception e)
        {
            Log.Write($"Couldn't prune old drops: {e.Message}");
        }
    }
}
