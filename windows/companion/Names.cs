namespace MwbBridgeCompanion;

/// File names for files arriving from the bridge. No Windows dependencies,
/// so the tests can check it on any OS.
static class Names
{
    /// `name` with characters Windows doesn't allow in a file name replaced.
    public static string Safe(string name)
    {
        var bad = Path.GetInvalidFileNameChars().Concat(new[] { '<', '>', ':', '"', '/', '\\', '|', '?', '*' }).ToHashSet();
        // Windows drops trailing dots and spaces, so "a.txt." is "a.txt".
        var clean = new string(name.Select(c => bad.Contains(c) || c < ' ' ? '_' : c).ToArray()).Trim().TrimEnd('.', ' ');
        return clean is "" or "." or ".." ? "file" : clean;
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
    /// always '/'; '\\' is accepted too): the folders are rebuilt, each part
    /// made safe, with empty, "." and ".." parts dropped so nothing lands
    /// outside. Each loose file is an item of its own, and every file under
    /// one offered folder name goes into the same rebuilt folder; top-level
    /// items get unique names. Returns each file's path in offer order, and
    /// the top-level items (what a drop or a paste hands on).
    public static (string[] Files, string[] TopLevel) Layout(IEnumerable<string> names)
    {
        var topUsed = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var fileUsed = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var folders = new Dictionary<string, string>(StringComparer.Ordinal);
        var files = new List<string>();
        var top = new List<string>();
        foreach (var raw in names)
        {
            var parts = raw.Split('/', '\\').Where(p => p.Length > 0 && p != "." && p != "..").ToArray();
            if (parts.Length == 0) parts = new[] { "file" };
            string path;
            if (parts.Length == 1)
            {
                path = MakeUnique(Safe(parts[0]), topUsed);
                top.Add(path);
            }
            else
            {
                if (!folders.TryGetValue(parts[0], out var folder))
                {
                    folder = MakeUnique(Safe(parts[0]), topUsed);
                    folders[parts[0]] = folder;
                    top.Add(folder);
                }
                var inside = parts.Skip(1).Select(Safe).ToArray();
                var dir = Path.Combine(new[] { folder }.Concat(inside[..^1]).ToArray());
                // Two offered files with the same path in one folder: keep both.
                var name = inside[^1];
                var stem = Path.GetFileNameWithoutExtension(name);
                var ext = Path.GetExtension(name);
                path = Path.Combine(dir, name);
                for (var n = 2; fileUsed.Contains(path); n++) path = Path.Combine(dir, $"{stem} ({n}){ext}");
            }
            fileUsed.Add(path);
            files.Add(path);
        }
        return (files.ToArray(), top.ToArray());
    }

    /// The files to offer for what was dragged or copied in Explorer: a file
    /// under its own name, and a folder as every file inside it, named by
    /// its path from the folder down with '/'. Links and junctions inside
    /// folders aren't followed (they can loop), and empty folders carry
    /// nothing. `skipped` hears about what couldn't be read.
    public static (string Path, string Name)[] Expand(IEnumerable<string> items, Action<string> skipped)
    {
        var result = new List<(string, string)>();
        var options = new EnumerationOptions { RecurseSubdirectories = true, IgnoreInaccessible = true, AttributesToSkip = FileAttributes.ReparsePoint };
        foreach (var item in items)
        {
            try
            {
                if (File.Exists(item))
                {
                    result.Add((item, Path.GetFileName(item)));
                }
                else if (Directory.Exists(item))
                {
                    var root = Path.GetFullPath(item).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
                    var name = Path.GetFileName(root);
                    foreach (var file in Directory.EnumerateFiles(root, "*", options).Order(StringComparer.Ordinal))
                    {
                        var relative = Path.GetRelativePath(root, file).Replace('\\', '/');
                        result.Add((file, $"{name}/{relative}"));
                    }
                }
                else
                {
                    skipped(item);
                }
            }
            catch (Exception)
            {
                skipped(item);
            }
        }
        return result.ToArray();
    }
}
