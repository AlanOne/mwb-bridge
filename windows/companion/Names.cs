namespace MwbBridgeCompanion;

/// File names for files arriving from the bridge. No Windows dependencies,
/// so the tests can check it on any OS.
static class Names
{
    /// `name` with characters Windows doesn't allow in a file name replaced,
    /// and a device name (CON, NUL, COM1...) prefixed so it's a plain file.
    public static string Safe(string name)
    {
        var bad = Path.GetInvalidFileNameChars().Concat(new[] { '<', '>', ':', '"', '/', '\\', '|', '?', '*' }).ToHashSet();
        // Windows drops trailing dots and spaces, so "a.txt." is "a.txt".
        var clean = new string(name.Select(c => bad.Contains(c) || c < ' ' ? '_' : c).ToArray()).Trim().TrimEnd('.', ' ');
        if (clean is "" or "." or "..") return "file";
        var stem = clean.Split('.')[0].TrimEnd(' ').ToUpperInvariant();
        var device = stem is "CON" or "PRN" or "AUX" or "NUL" || (stem.Length == 4 && (stem.StartsWith("COM") || stem.StartsWith("LPT")) && char.IsAsciiDigit(stem[3]) && stem[3] != '0');
        return device ? "_" + clean : clean;
    }

    /// Safe names for one offer's files, made unique (Windows file names
    /// ignore case) the way Explorer does it: "a.txt", "a (2).txt", ...
    public static string[] Unique(IEnumerable<string> names)
    {
        var used = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        return names.Select(raw => MakeUnique(Safe(raw), used)).ToArray();
    }

    /// `name`, or "stem (2).ext" and so on, whichever `used` doesn't have
    /// yet; then it does.
    static string MakeUnique(string name, HashSet<string> used)
    {
        var stem = Path.GetFileNameWithoutExtension(name);
        var ext = Path.GetExtension(name);
        var candidate = name;
        for (var n = 2; !used.Add(candidate); n++) candidate = $"{stem} ({n}){ext}";
        return candidate;
    }

    /// Where one offer's files go, relative to its folder. A name can be a
    /// path from a dragged or copied folder down ("photos/2026/a.jpg",
    /// always '/'; '\\' is accepted from older senders): the folders are
    /// rebuilt, each part made safe, with empty, "." and ".." parts dropped
    /// so nothing lands outside. Every offered folder keeps one place, and
    /// any name that would land on something already there (a file where a
    /// folder goes, names Windows sees as the same: other case, a trailing
    /// dot, a character made safe) gets " (2)" and so on, the way Explorer
    /// does it. Returns each file's path in offer order, and the top-level
    /// items (what a drop or a paste hands on).
    public static (string[] Files, string[] TopLevel) Layout(IEnumerable<string> names)
    {
        var taken = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var folders = new Dictionary<string, string>(StringComparer.Ordinal);
        var files = new List<string>();
        var top = new List<string>();
        foreach (var raw in names)
        {
            var parts = raw.Split('/', '\\').Where(p => p.Length > 0 && p != "." && p != "..").ToArray();
            if (parts.Length == 0) parts = new[] { "file" };
            var parent = "";
            var rawPrefix = "";
            for (var i = 0; i < parts.Length - 1; i++)
            {
                rawPrefix += "/" + parts[i];
                if (!folders.TryGetValue(rawPrefix, out var folder))
                {
                    folder = Claim(parent, Safe(parts[i]), taken);
                    folders[rawPrefix] = folder;
                    if (i == 0) top.Add(folder);
                }
                parent = folder;
            }
            var file = Claim(parent, Safe(parts[^1]), taken);
            if (parts.Length == 1) top.Add(file);
            files.Add(file);
        }
        return (files.ToArray(), top.ToArray());
    }

    /// `parent\name`, or "stem (2).ext" and so on inside `parent`, whichever
    /// nothing in `taken` has yet; then it has.
    static string Claim(string parent, string name, HashSet<string> taken)
    {
        var stem = Path.GetFileNameWithoutExtension(name);
        var ext = Path.GetExtension(name);
        var path = parent == "" ? name : Path.Combine(parent, name);
        for (var n = 2; !taken.Add(path); n++)
        {
            var candidate = $"{stem} ({n}){ext}";
            path = parent == "" ? candidate : Path.Combine(parent, candidate);
        }
        return path;
    }

    /// How many files one offer may carry: past that, a dragged folder
    /// (a whole drive, a junction to one) is cut short and says so.
    public const int MaxOfferFiles = 50_000;

    /// The files to offer for what was dragged or copied in Explorer: a file
    /// under its own name, and a folder as every file inside it, named by
    /// its path from the folder down with '/'. Inside folders, links and
    /// junctions to folders aren't followed (they can loop) but links to
    /// files are. Empty folders carry nothing. `skipped` hears about what
    /// couldn't be read or was left out. Slow for a big folder: call it off
    /// the UI thread.
    public static (string Path, string Name, ulong Size)[] Expand(IEnumerable<string> items, Action<string> skipped)
    {
        var result = new List<(string, string, ulong)>();
        void Add(string path, string name)
        {
            ulong size = 0;
            try { size = (ulong)new FileInfo(path).Length; } catch (Exception) { }
            result.Add((path, name, size));
        }
        foreach (var item in items)
        {
            try
            {
                if (File.Exists(item))
                {
                    Add(item, Path.GetFileName(item));
                    continue;
                }
                if (!Directory.Exists(item))
                {
                    skipped(item);
                    continue;
                }
                var root = Path.GetFullPath(item).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
                var pending = new Stack<(string Dir, string Name)>();
                pending.Push((root, Path.GetFileName(root)));
                while (pending.Count > 0 && result.Count < MaxOfferFiles)
                {
                    var (dir, name) = pending.Pop();
                    string[] entries;
                    try { entries = Directory.GetFileSystemEntries(dir); }
                    catch (Exception) { skipped(dir); continue; }
                    Array.Sort(entries, StringComparer.Ordinal);
                    var subdirs = new List<(string, string)>();
                    foreach (var entry in entries)
                    {
                        var entryName = $"{name}/{Path.GetFileName(entry)}";
                        FileAttributes attributes;
                        try { attributes = File.GetAttributes(entry); }
                        catch (Exception) { skipped(entry); continue; }
                        if (!attributes.HasFlag(FileAttributes.Directory)) Add(entry, entryName);
                        else if (attributes.HasFlag(FileAttributes.ReparsePoint)) skipped(entry);
                        else subdirs.Add((entry, entryName));
                    }
                    // Pushed in reverse so they're walked in name order.
                    for (var i = subdirs.Count - 1; i >= 0; i--) pending.Push(subdirs[i]);
                }
                if (pending.Count > 0 || result.Count >= MaxOfferFiles) skipped($"the rest of {item} (more than {MaxOfferFiles} files)");
            }
            catch (Exception)
            {
                skipped(item);
            }
        }
        return result.ToArray();
    }
}
