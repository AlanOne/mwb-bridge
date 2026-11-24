using System.Text.Json;

namespace MwbBridgeCompanion;

/// Whether a bridge is set up in PowerToys' Mouse Without Borders: it has
/// to be in both `MachineMatrixString` (the layout edge crossing follows)
/// and `MachinePool` (the list the switch hotkeys resolve names against),
/// which have to be hand-edited (see the README). No Windows dependencies,
/// so the tests can check it on any OS.
static class SetupCheck
{
    /// What's missing for `bridge` in Mouse Without Borders' settings
    /// (`properties`, the settings file's "properties" object), or null when
    /// nothing is, or when the settings can't be read well enough to tell.
    public static string? Problem(JsonElement properties, string bridge)
    {
        try
        {
            var missing = new List<string>();
            var matrix = MatrixNames(properties);
            var pool = PoolNames(properties);
            if (matrix == null || pool == null) return null;
            if (!matrix.Contains(bridge, StringComparer.OrdinalIgnoreCase)) missing.Add("MachineMatrixString (the layout)");
            if (!pool.Contains(bridge, StringComparer.OrdinalIgnoreCase)) missing.Add("MachinePool");
            return missing.Count == 0
                ? null
                : $"{bridge} isn't in Mouse Without Borders' {string.Join(" or ", missing)}. Add it to settings.json with PowerToys closed (see the README's \"On the Windows side\").";
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// The layout's names: stored as a plain list, or wrapped in "value".
    static string[]? MatrixNames(JsonElement properties)
    {
        if (!properties.TryGetProperty("MachineMatrixString", out var m)) return null;
        if (m.ValueKind == JsonValueKind.Object && m.TryGetProperty("value", out var v)) m = v;
        return m.ValueKind switch
        {
            JsonValueKind.Array => m.EnumerateArray().Select(e => e.GetString() ?? "").ToArray(),
            JsonValueKind.String => (m.GetString() ?? "").Split(','),
            _ => null,
        };
    }

    /// The pool's names, from "Name:ID,Name:ID,...".
    static string[]? PoolNames(JsonElement properties)
    {
        if (!properties.TryGetProperty("MachinePool", out var p)) return null;
        if (p.ValueKind == JsonValueKind.Object && p.TryGetProperty("value", out var v)) p = v;
        if (p.ValueKind != JsonValueKind.String) return null;
        return (p.GetString() ?? "").Split(',').Select(slot => slot.Split(':')[0]).ToArray();
    }
}
