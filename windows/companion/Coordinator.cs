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
    public Func<bool>? BridgeConnected { get; set; }

    // Windows -> Mac drags offered to the bridge, by id: the files it fetches.
    readonly ConcurrentDictionary<ulong, (string Path, string Name)[]> _toMac = new();
    // Mac -> Windows drags, by id.
    readonly ConcurrentDictionary<ulong, IncomingDrop> _fromMac = new();
    ulong _nextId = (ulong)DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    // Windows -> Mac state.
    bool _catching;      // the catcher is up for a drag that left Windows
    // The bridge's machine has control (ACTIVE came, INACTIVE not yet):
    // anything injected now is forwarded there by Mouse Without Borders.
    bool _bridgeActive;
    // A press injected for a drag that didn't start, whose release is
    // owed once Windows has control again.
    bool _releaseOwed;
    bool _caughtDrag;    // a drag is hovering over it
    bool _offered;       // its files were offered to the bridge
    bool _leftoverEnded; // Windows' drag already ended on the catcher
    // Bumped whenever the catcher goes up or down, so a timer left over
    // from an earlier catch can't act on a newer one.
    int _catchSession;

    // Mac -> Windows state.
    IncomingDrop? _pendingSource; // waiting for the simulated press to land
    bool _dragRunning;            // a drag from the bridge is under way here
    // When EndLeftoverDrag released Windows' leftover drag, until it has
    // ended on the catcher: a drag started here meanwhile waits for that.
    DateTime? _endingLeftoverSince;
    static readonly TimeSpan LeftoverEndWait = TimeSpan.FromMilliseconds(500);

    public Coordinator(Control ui)
    {
        _ui = ui;
        _catcher.FilesCaught += OnFilesCaught;
        _catcher.DragEntered += keyState =>
        {
            if (!_caughtDrag) Log.Write($"[to-mac] A drag entered the catcher (key state {keyState}: left button {((keyState & 1) != 0 ? "down" : "up")} as the drag sees it).");
            _caughtDrag = true;
        };
        _catcher.Dropped += () =>
        {
            Log.Write("[to-mac] The leftover Windows drag ended on the catcher.");
            _endingLeftoverSince = null;
            if (_catching) _leftoverEnded = true;
        };
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
        _bridgeActive = true;
        if (_catching) return;
        Log.Write($"[to-mac] The bridge's machine is active (left button {(Native.LeftButtonDown() ? "down" : "up")} as Windows reports it), watching for a drag.");
        _catching = true;
        _caughtDrag = _offered = _leftoverEnded = false;
        var session = ++_catchSession;
        _catcher.ShowOver(SystemInformation.VirtualScreen);
        _raise.Start();
        // Not every held-button crossing is a drag of something (moving a
        // window to the edge): give up if no drag shows up.
        After(1500, () =>
        {
            if (session == _catchSession && _catching && !_caughtDrag)
            {
                HideCatcher();
            }
        });
    }

    /// Runs `action` on the UI thread after `ms` milliseconds.
    static void After(int ms, Action action)
    {
        var timer = new System.Windows.Forms.Timer { Interval = ms };
        timer.Tick += (_, _) => { timer.Dispose(); action(); };
        timer.Start();
    }

    void OnFilesCaught(string[] dropped)
    {
        // Once per catch: the drag can leave and re-enter the catcher, and a
        // second offer would start a second drag on the Mac.
        if (!_catching || _offered) return;
        // Taken now, so a re-entry while the folders are walked doesn't
        // offer twice; given back if the offer couldn't be sent.
        _offered = true;
        var session = _catchSession;
        // Folders go as every file inside them (see Names.Expand), which can
        // take a while: not on this thread, which OLE's DragEnter is on.
        Task.Run(() =>
        {
            var files = Names.Expand(dropped, skipped => Log.Write($"[to-mac] Skipping {skipped}."));
            // Sent from the UI thread, and only if this catch is still the
            // one going on: a drag that ended (or came back and started
            // over) during the walk mustn't start a drag of it on the Mac.
            Post(() =>
            {
                if (session != _catchSession || !_catching)
                {
                    Log.Write("[to-mac] That drag ended while its folders were walked: not offering it.");
                    return;
                }
                if (files.Length == 0)
                {
                    Log.Write("[to-mac] Nothing to send in that drag (only empty folders?).");
                    return;
                }
                if (Offer(Frames.OfferToMac, files, "drag", out var id)) return;
                // No bridge right now. OLE won't enter the catcher again
                // while the drag stays over it, so try again from here.
                Log.Write($"[to-mac] Couldn't send drag {id} to the bridge, trying again in a second.");
                _offered = false;
                After(1000, () => { if (session == _catchSession) OnFilesCaught(dropped); });
            });
        });
    }

    /// Offers `files` to the bridge as a new id (kept for its "get").
    bool Offer(uint frame, (string Path, string Name, ulong Size)[] files, string what, out ulong id)
    {
        id = (ulong)Interlocked.Increment(ref _nextId);
        _toMac[id] = files.Select(f => (f.Path, f.Name)).ToArray();
        var offer = new OfferMsg(id, files.Select(f => new FileInfoMsg(f.Name, f.Size)).ToList());
        Log.Write($"[to-mac] Offering {files.Length} file(s) to the bridge as {what} {id}: {string.Join(", ", offer.Files.Take(20).Select(f => f.Name))}{(files.Length > 20 ? ", ..." : "")}");
        return SendToBridge?.Invoke(frame, offer) == true;
    }

    /// Control came back to Windows (`version`: the bridge's protocol
    /// version, which says how to read the message).
    public void OnInactive(InactiveMsg inactive, uint version)
    {
        _bridgeActive = false;
        if (_releaseOwed)
        {
            _releaseOwed = false;
            if (_catching && inactive.DragCameBack(version))
            {
                // The button is really held for the drag coming back: its
                // own release ends it, and an injected one would drop it.
                Log.Write("[to-windows] A drag came back holding the button: the press a drag that didn't start left behind is let go with it.");
            }
            else
            {
                Native.LeftButton(down: false);
                Log.Write("[to-windows] Windows has control again: letting go of the press a drag that didn't start left behind.");
            }
        }
        if (!_catching) return;
        if (inactive.DragCameBack(version))
        {
            // Dragged back to Windows without letting go over there: let
            // the drag carry on here.
            Log.Write("[to-mac] The drag came back to Windows, handing it back.");
            HideCatcher();
            return;
        }
        if (!_caughtDrag)
        {
            // Nothing reached the catcher. (A drag let go over there within
            // the moment OLE takes to enter the catcher is missed this way;
            // watching longer for it misfired on plain clicks and quick
            // returns, so it isn't.)
            HideCatcher();
            return;
        }
        Log.Write("[to-mac] Released on the other machine, ending the leftover Windows drag.");
        EndLeftoverDrag();
    }

    /// The drag the catcher caught ended on the other machine (or the
    /// bridge went away), but Windows' drag is still going: release it
    /// here over the catcher, which ends it harmlessly. Hiding the catcher
    /// instead would let it drop on whatever is under the cursor.
    void EndLeftoverDrag()
    {
        // Released right away, while Windows surely has control: anything
        // injected once control is back on the other machine would be
        // forwarded there. The catch ends now too, so a quick return to the
        // other machine starts a new one; the catcher only stays up long
        // enough for the release to land on it, unless that new catch
        // needs it.
        _catcher.ShowOver(SystemInformation.VirtualScreen);
        Native.LeftButton(down: false);
        _endingLeftoverSince = DateTime.UtcNow;
        _catching = _caughtDrag = _offered = _leftoverEnded = false;
        var session = ++_catchSession;
        After(300, () =>
        {
            if (session != _catchSession) return;
            _raise.Stop();
            _catcher.Hide();
        });
    }

    void HideCatcher()
    {
        _raise.Stop();
        _catcher.Hide();
        _catching = _caughtDrag = _offered = _leftoverEnded = false;
        _catchSession++;
    }

    /// The bridge fetching a drag's files.
    public (string Path, string Name)[]? FilesToMac(ulong id) => _toMac.TryGetValue(id, out var f) ? f : null;

    // ---- Mac -> Windows ------------------------------------------------

    /// Files dragged on the Mac came to Windows: start a Windows drag of
    /// them under the cursor while the bytes arrive.
    // ---- File copy and paste ----------------------------------------

    string[] _lastSetOnClipboard = Array.Empty<string>();
    string[] _lastOffered = Array.Empty<string>();
    // Moves on with every copy (changed on the UI thread only), so a walk
    // or a Mac copy still arriving can tell it was overtaken.
    volatile int _copyGeneration;
    // Counts copies from the Mac: only the latest goes on the clipboard.
    long _macCopies;
    DateTime _lastOfferedAt;

    /// Files copied on the Mac: put them on Windows' clipboard once they've
    /// all arrived, as if copied in Explorer.
    public IncomingDrop RegisterCopy(OfferMsg offer)
    {
        var drop = IncomingDrop.Create(offer, IncomingDrop.CopiesRoot);
        _fromMac[offer.Id] = drop;
        // A slow copy mustn't land over anything copied since, on the Mac
        // (a later copy from it) or here.
        var mine = Interlocked.Increment(ref _macCopies);
        var clipboardAt = _copyGeneration;
        drop.Completed = () => Post(() =>
        {
            if (mine != Interlocked.Read(ref _macCopies) || clipboardAt != _copyGeneration)
            {
                Log.Write($"[copy] {drop.TopLevel.Length} item(s) copied on the Mac arrived after something newer was copied: not putting them on the clipboard.");
                return;
            }
            try
            {
                var list = new System.Collections.Specialized.StringCollection();
                list.AddRange(drop.TopLevel);
                Clipboard.SetFileDropList(list);
                _lastSetOnClipboard = drop.TopLevel;
                Log.Write($"[copy] {drop.TopLevel.Length} item(s) copied on the Mac are on the clipboard.");
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
            if (!Clipboard.ContainsFileDropList())
            {
                // Something else copied: a file copy still being walked is
                // stale now.
                _copyGeneration++;
                return;
            }
            files = Clipboard.GetFileDropList().Cast<string>().ToArray();
        }
        catch (Exception e)
        {
            Log.Write($"[copy] Couldn't read the clipboard: {e.Message}");
            return;
        }
        // Our own paste of the Mac's files, or Explorer announcing the same
        // copy twice in a row.
        if (files.SequenceEqual(_lastSetOnClipboard))
        {
            _copyGeneration++; // newer than any copy still being walked
            return;
        }
        if (files.SequenceEqual(_lastOffered) && DateTime.Now - _lastOfferedAt < TimeSpan.FromSeconds(2)) return;
        _lastOffered = files;
        _lastOfferedAt = DateTime.Now;
        var copy = ++_copyGeneration;
        // Folders go as every file inside them (see Names.Expand), which can
        // take a while: not on the UI thread.
        Task.Run(() =>
        {
            var sendable = Names.Expand(files, skipped => Log.Write($"[copy] Skipping {skipped}."));
            if (sendable.Length == 0) return;
            // Only if nothing newer was copied during the walk.
            Post(() =>
            {
                if (copy != _copyGeneration)
                {
                    Log.Write("[copy] Something newer was copied while that copy's folders were walked: not offering it.");
                    return;
                }
                if (!Offer(Frames.ClipboardToMac, sendable, "copy", out var id)) Log.Write($"[copy] Couldn't offer copy {id}: no bridge connected.");
            });
        });
    }

    public IncomingDrop Register(OfferMsg offer)
    {
        var drop = IncomingDrop.Create(offer);
        _fromMac[offer.Id] = drop;
        return drop;
    }

    public void OnOfferToWindows(IncomingDrop drop) => StartWhenLeftoverEnded(drop, waiting: false);

    /// A drag from the Mac usually arrives right as Windows' leftover drag
    /// (one that went to the Mac before) is being ended: until that drag
    /// has ended, a press goes to it rather than to the source window, and
    /// the new drag never starts (live-found 2026-11-24). So wait for it,
    /// briefly.
    void StartWhenLeftoverEnded(IncomingDrop drop, bool waiting)
    {
        if (drop.Cancelled) return;
        if (_endingLeftoverSince is DateTime since && DateTime.UtcNow - since < LeftoverEndWait)
        {
            if (!waiting) Log.Write($"[to-windows] Drag {drop.Id}: waiting for the leftover Windows drag to end first.");
            After(30, () => StartWhenLeftoverEnded(drop, waiting: true));
            return;
        }
        _endingLeftoverSince = null;
        if (_bridgeActive)
        {
            // Control went back to the other machine meanwhile: a press now
            // would be forwarded there (and look like a drop over there).
            Log.Write($"[to-windows] Drag {drop.Id}: control went back to the bridge's machine before it could start here, saving its files to the desktop instead.");
            drop.SaveToDesktopWhenComplete();
            return;
        }
        // Windows has control: no catch needs the catcher, and over the
        // whole desktop it could take the press meant for the source.
        if (!_catching)
        {
            _raise.Stop();
            _catcher.Hide();
        }
        StartSourceDrag(drop);
    }

    void StartSourceDrag(IncomingDrop drop)
    {
        if (_pendingSource != null || _dragRunning)
        {
            // One is being started or is running: leave it be.
            Log.Write($"[to-windows] Drag {drop.Id}: another drag from the bridge is still going on here, saving its files to the desktop instead.");
            drop.SaveToDesktopWhenComplete();
            return;
        }
        Log.Write($"[to-windows] Drag {drop.Id}: {drop.Paths.Length} file(s) from the bridge, starting a drag here.");
        Log.Write($"[to-windows] Cursor at {Cursor.Position}, left button {(Native.LeftButtonDown() ? "down" : "up")} as Windows sees it.");
        if (!Native.LeftButtonDown())
        {
            // Windows doesn't know about the button held on the other
            // machine; this press, on the source window, gives it one.
            // Over the whole desktop, not just around the cursor: the
            // cursor is often still moving fast from the crossing, and a
            // press injected at one spot lands wherever it is by the time
            // Windows handles it (live-found 2026-11-24: 224px in 70ms,
            // past a 64px window, so the drag never started).
            _pendingSource = drop;
            _source.ShowOver(SystemInformation.VirtualScreen);
            var pressed = false;
            var press = new System.Windows.Forms.Timer { Interval = 40 };
            press.Tick += (_, _) =>
            {
                press.Dispose();
                if (_pendingSource != drop) return;
                if (_bridgeActive)
                {
                    _pendingSource = null;
                    _source.Hide();
                    Log.Write($"[to-windows] Drag {drop.Id}: control went back to the bridge's machine before the press, saving its files to the desktop instead.");
                    drop.SaveToDesktopWhenComplete();
                    return;
                }
                // Again right before pressing: other topmost windows (Mouse
                // Without Borders keeps one at the cursor) may have come up.
                var now = Cursor.Position;
                _source.ShowOver(SystemInformation.VirtualScreen);
                Log.Write($"[to-windows] Pressing at {now} on {Native.DescribeWindowAt(now)} (the source window is 0x{_source.Handle.ToInt64():x}).");
                Native.LeftButton(down: true);
                pressed = true;
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
                    // Never injected while the other machine has control:
                    // it would get the release. Owed until Windows has it.
                    if (pressed && _bridgeActive) _releaseOwed = true;
                    else if (pressed) Native.LeftButton(down: false);
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
        var data = new DataObject(DataFormats.FileDrop, drop.TopLevel);
        DateTime? releasedAt = null;
        var gaveUp = false;
        void Query(object? sender, QueryContinueDragEventArgs e)
        {
            if (e.EscapePressed || drop.Cancelled)
            {
                e.Action = DragAction.Cancel;
            }
            else if ((e.KeyState & 1) == 0)
            {
                if (releasedAt == null)
                {
                    releasedAt = DateTime.UtcNow;
                    if (!drop.IsComplete) Log.Write($"[to-windows] Drag {drop.Id} let go, waiting up to {EarlyReleaseWait.TotalSeconds:0.#}s for its files to finish arriving.");
                }
                // Hold the drop until every file has fully arrived, but not
                // for long: the cursor moves on meanwhile, and the drop goes
                // wherever it is by then.
                if (drop.IsComplete) e.Action = DragAction.Drop;
                else if (drop.Failed) e.Action = DragAction.Cancel;
                else if (DateTime.UtcNow - releasedAt > EarlyReleaseWait)
                {
                    gaveUp = true;
                    e.Action = DragAction.Cancel;
                }
                else e.Action = DragAction.Continue;
            }
        }
        // OLE only asks QueryContinueDrag again when a message arrives; a
        // timer makes sure one does while the files finish arriving.
        using var poll = new System.Windows.Forms.Timer { Interval = 50 };
        poll.Tick += (_, _) => { };
        poll.Start();
        _source.QueryContinueDrag += Query;
        DragDropEffects effect;
        _dragRunning = true;
        try { effect = _source.DoDragDrop(data, DragDropEffects.Copy); }
        finally { _dragRunning = false; }
        _source.QueryContinueDrag -= Query;
        poll.Stop();
        _source.Hide();
        if (gaveUp)
        {
            Log.Write($"[to-windows] Drag {drop.Id}: its files were still arriving, saving them to the desktop instead.");
            drop.SaveToDesktopWhenComplete();
        }
        else
        {
            Log.Write($"[to-windows] Drag {drop.Id} ended: {(effect == DragDropEffects.None ? "not dropped anywhere that takes it (or cancelled)" : "dropped into an app")}.");
        }
    }

    // How long a release let go before the files have all arrived is held.
    static readonly TimeSpan EarlyReleaseWait = TimeSpan.FromSeconds(1);

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

    /// After a disconnect mid-catch: once the parked cursor moves (Windows
    /// has control), end Windows' leftover drag, unless the catch has
    /// ended some other way first. A bridge that reconnected meanwhile gets
    /// half a second to settle it with its INACTIVE (the drag may be coming
    /// back), since it sends one when control leaves its machine.
    void WaitForControlThenEnd(int session, Point parked)
    {
        var poll = new System.Windows.Forms.Timer { Interval = 100 };
        poll.Tick += (_, _) =>
        {
            if (session != _catchSession || !_catching)
            {
                poll.Dispose();
                return;
            }
            if (Cursor.Position == parked) return;
            poll.Dispose();
            if (_leftoverEnded)
            {
                HideCatcher();
            }
            else if (BridgeConnected?.Invoke() == true)
            {
                After(500, () =>
                {
                    if (session != _catchSession || !_catching) return;
                    Log.Write("[to-mac] Windows has control again and the bridge said nothing, ending the leftover Windows drag.");
                    EndLeftoverDrag();
                });
            }
            else
            {
                Log.Write("[to-mac] Windows has control again, ending the leftover Windows drag.");
                EndLeftoverDrag();
            }
        };
        poll.Start();
    }

    public IncomingDrop? FromMac(ulong id) => _fromMac.TryGetValue(id, out var d) ? d : null;

    public void OnBridgeDisconnected()
    {
        // A drag caught here is still going on Windows, but releasing it
        // now isn't safe: no INACTIVE came, so the other machine may still
        // have control, and an injected release would be forwarded there.
        // Hiding the catcher would let the drag drop wherever the cursor
        // is. So the catcher stays up (harmless while the cursor is parked)
        // until Windows has control again, which shows as the parked cursor
        // moving.
        if (_catching && _caughtDrag && !_leftoverEnded)
        {
            Log.Write("[to-mac] The bridge went away during a caught drag; keeping the catcher up until Windows has control again.");
            WaitForControlThenEnd(_catchSession, Cursor.Position);
        }
        else if (_catching) HideCatcher();
        foreach (var drop in _fromMac.Values) drop.Cancelled = true;
        // No INACTIVE will come to settle an owed release: let go once
        // Windows has control again (the parked cursor moves).
        _bridgeActive = false;
        if (_releaseOwed) ReleaseOwedOnceControlBack(Cursor.Position);
    }

    void ReleaseOwedOnceControlBack(Point parked)
    {
        var poll = new System.Windows.Forms.Timer { Interval = 100 };
        poll.Tick += (_, _) =>
        {
            if (!_releaseOwed) { poll.Dispose(); return; }
            if (Cursor.Position == parked) return;
            poll.Dispose();
            _releaseOwed = false;
            Native.LeftButton(down: false);
            Log.Write("[to-windows] Windows has control again after the bridge went away: letting go of the press a drag that didn't start left behind.");
        };
        poll.Start();
    }
}

/// A Mac -> Windows drag's files, written as they arrive.
sealed class IncomingDrop
{
    public ulong Id { get; }
    /// Every file, in offer order (the bytes arrive by index).
    public string[] Paths { get; }
    /// What a drop or a paste hands on: loose files and rebuilt folders.
    public string[] TopLevel { get; }
    /// All the files' bytes together, for the transfer's progress.
    public long Total { get; }
    readonly bool[] _done;
    public volatile bool Cancelled;
    public volatile bool Failed;
    // Both under _done's lock: the last file can arrive (network thread)
    // just as the drag gives up (UI thread), and only one may save.
    bool _saveToDesktop;
    bool _saved;

    public bool IsComplete { get { lock (_done) return _done.All(d => d); } }

    IncomingDrop(ulong id, string[] paths, string[] topLevel, long total)
    {
        Id = id;
        Total = total;
        Paths = paths;
        TopLevel = topLevel;
        _done = new bool[paths.Length];
    }

    public static string Root { get; } = Path.Combine(Path.GetTempPath(), "MwbBridgeCompanion", "drops");
    public static string CopiesRoot { get; } = Path.Combine(Path.GetTempPath(), "MwbBridgeCompanion", "copies");

    /// Runs (on a network thread) once every file has arrived.
    public Action? Completed { get; set; }

    public static IncomingDrop Create(OfferMsg offer, string? root = null)
    {
        PruneSoon();
        var dir = Path.Combine(root ?? Root, offer.Id.ToString());
        Directory.CreateDirectory(dir);
        var (files, top) = Names.Layout(offer.Files.Select(f => f.Name));
        var paths = files.Select(f => Path.Combine(dir, f)).ToArray();
        // Empty placeholders, so the drag can start before the bytes arrive.
        foreach (var p in paths)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(p)!);
            File.WriteAllBytes(p, Array.Empty<byte>());
        }
        var total = offer.Files.Aggregate(0L, (sum, f) => sum + (long)Math.Min(f.Size, (ulong)long.MaxValue / 2));
        return new IncomingDrop(offer.Id, paths, top.Select(t => Path.Combine(dir, t)).ToArray(), total);
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
            SaveIfWanted();
        }
    }

    public void SaveToDesktopWhenComplete()
    {
        lock (_done) _saveToDesktop = true;
        SaveIfWanted();
    }

    void SaveIfWanted()
    {
        lock (_done)
        {
            if (!_saveToDesktop || _saved || !_done.All(d => d)) return;
            _saved = true;
        }
        try
        {
            SaveToDesktop();
        }
        catch (Exception e)
        {
            Log.Write($"[to-windows] Drag {Id}: couldn't save to the desktop: {e.Message}");
        }
    }

    void SaveToDesktop()
    {
        var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Desktop), "MouseWithoutBorders");
        Directory.CreateDirectory(dir);
        foreach (var p in TopLevel) CopyAll(p, SaveName(dir, Path.GetFileName(p), Directory.Exists(p)));
        Log.Write($"[to-windows] Drag {Id}: saved to {dir}.");
        Process.Start(new ProcessStartInfo(dir) { UseShellExecute = true });
    }

    /// Where to save `name` in `dir`: a file replaces a file of the same name,
    /// as before, but a folder never merges into one already there (and
    /// never lands on a file, or a file on a folder): it gets " (2)" and so on.
    static string SaveName(string dir, string name, bool isFolder)
    {
        var path = Path.Combine(dir, name);
        bool Free(string p) => isFolder ? !Directory.Exists(p) && !File.Exists(p) : !Directory.Exists(p);
        var stem = isFolder ? name : Path.GetFileNameWithoutExtension(name);
        var ext = isFolder ? "" : Path.GetExtension(name);
        for (var n = 2; !Free(path); n++) path = Path.Combine(dir, $"{stem} ({n}){ext}");
        return path;
    }

    /// Copies a file, or a folder with everything in it.
    static void CopyAll(string from, string to)
    {
        if (!Directory.Exists(from))
        {
            File.Copy(from, to, overwrite: true);
            return;
        }
        Directory.CreateDirectory(to);
        foreach (var entry in Directory.EnumerateFileSystemEntries(from)) CopyAll(entry, Path.Combine(to, Path.GetFileName(entry)));
    }

    static int _pruning;
    static DateTime _lastPrune = DateTime.MinValue;

    /// `PruneOld` in the background, at most hourly: on the offer's path
    /// it would hold up the control connection, and the files' data
    /// connection only waits about 2s for the offer to be registered.
    static void PruneSoon()
    {
        if (DateTime.UtcNow - _lastPrune < TimeSpan.FromHours(1)) return;
        if (Interlocked.Exchange(ref _pruning, 1) == 1) return;
        _lastPrune = DateTime.UtcNow;
        Task.Run(() =>
        {
            try { PruneOld(); }
            finally { Interlocked.Exchange(ref _pruning, 0); }
        });
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
