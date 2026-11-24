namespace MwbBridgeCompanion;

/// One file transfer with the bridge, as `Transfers` tracks it.
sealed class Transfer
{
    public ulong Id { get; }
    public bool Sending { get; }
    public long Total { get; }
    public DateTime Started { get; } = DateTime.UtcNow;
    long _done;
    public long Done => Interlocked.Read(ref _done);
    public volatile bool Cancelled;
    /// Cuts its data connection.
    public Action Abort { get; }

    public Transfer(ulong id, bool sending, long total, Action abort)
    {
        Id = id;
        Sending = sending;
        Total = total;
        Abort = abort;
    }

    public void Add(long n) => Interlocked.Add(ref _done, n);
}

/// The transfers going on, for the tray's progress and "Cancel transfer".
/// Mirrors `Transfers` in daemon/protocol/src/companion.rs.
static class Transfers
{
    static readonly object Lock = new();
    static readonly Dictionary<ulong, Transfer> Active = new();
    // Offers cancelled lately, so a transfer of one that only starts
    // afterwards is refused. The last few are kept.
    static readonly LinkedList<ulong> Cancelled = new();
    const int RememberedCancels = 64;
    /// Transfers that finish this quickly are never shown.
    public static readonly TimeSpan ShowAfter = TimeSpan.FromSeconds(1);

    /// Starts tracking offer `id`'s transfer; null if it was cancelled
    /// already. `abort` cuts its connection on a cancel.
    public static Transfer? Begin(ulong id, bool sending, long total, Action abort)
    {
        lock (Lock)
        {
            if (Cancelled.Contains(id)) return null;
            var t = new Transfer(id, sending, total, abort);
            Active[id] = t;
            return t;
        }
    }

    public static void End(Transfer t)
    {
        lock (Lock)
        {
            if (Active.TryGetValue(t.Id, out var current) && current == t) Active.Remove(t.Id);
        }
    }

    /// Cancels offer `id`: a running transfer of it is cut, one that hasn't
    /// started is refused when it does. Whether one was running.
    public static bool Cancel(ulong id)
    {
        Transfer? t;
        lock (Lock)
        {
            if (!Cancelled.Contains(id))
            {
                if (Cancelled.Count == RememberedCancels) Cancelled.RemoveFirst();
                Cancelled.AddLast(id);
            }
            if (!Active.TryGetValue(id, out t)) return false;
            t.Cancelled = true;
        }
        try { t.Abort(); } catch (Exception e) { Log.Write($"Cutting transfer {id} failed: {e.Message}"); }
        return true;
    }

    public static bool WasCancelled(ulong id)
    {
        lock (Lock) return Cancelled.Contains(id);
    }

    public static Transfer[] Snapshot()
    {
        lock (Lock) return Active.Values.OrderBy(t => t.Started).ThenBy(t => t.Id).ToArray();
    }

    /// One line about the transfers that have run for a second or more,
    /// with the bridge's machine called `peer`, and the overall percentage;
    /// null while there are none. As `progress_line` on the bridge.
    public static (string Line, int Percent)? ProgressLine(IReadOnlyList<(bool Sending, long Done, long Total, TimeSpan RunningFor)> list, string peer)
    {
        var shown = list.Where(t => t.RunningFor >= ShowAfter).ToList();
        if (shown.Count == 0) return null;
        long done = shown.Sum(t => t.Done), total = shown.Sum(t => t.Total);
        var pct = Percent(done, total);
        var sending = shown.Count(t => t.Sending);
        var n = shown.Count;
        var what = (n, sending) switch
        {
            (1, 1) => $"Sending to {peer}",
            (1, _) => $"Receiving from {peer}",
            _ when sending == n => $"Sending {n} transfers to {peer}",
            (_, 0) => $"Receiving {n} transfers from {peer}",
            _ => $"{n} transfers with {peer}",
        };
        return ($"{what}: {FormatSize(done)} of {FormatSize(total)} ({pct}%)", pct);
    }

    public static (string Line, int Percent)? ProgressLine(string peer)
    {
        var now = DateTime.UtcNow;
        return ProgressLine(Snapshot().Select(t => (t.Sending, t.Done, t.Total, now - t.Started)).ToList(), peer);
    }

    public static int Percent(long done, long total) =>
        total <= 0 ? (done == 0 ? 0 : 100) : (int)(Math.Min(done, total) * 100 / total);

    /// KB, MB or GB, as the bridge's `format_size`.
    public static string FormatSize(long bytes)
    {
        const double KB = 1024, MB = KB * 1024, GB = MB * 1024;
        double b = bytes;
        if (b < MB) return $"{Math.Ceiling(b / KB):0} KB";
        if (b < 10 * MB) return (b / MB).ToString("0.0", System.Globalization.CultureInfo.InvariantCulture) + " MB";
        if (b < GB) return $"{Math.Round(b / MB):0} MB";
        return (b / GB).ToString("0.0", System.Globalization.CultureInfo.InvariantCulture) + " GB";
    }
}
