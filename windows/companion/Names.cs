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
        var result = new List<string>();
        foreach (var raw in names)
        {
            var name = Safe(raw);
            var stem = Path.GetFileNameWithoutExtension(name);
            var ext = Path.GetExtension(name);
            for (var n = 2; !used.Add(name); n++) name = $"{stem} ({n}){ext}";
            result.Add(name);
        }
        return result.ToArray();
    }
}
