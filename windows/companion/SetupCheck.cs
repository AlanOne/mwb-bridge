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
    /// Read the way PowerToys reads them, so a layout or pool it would throw
    /// away counts as missing.
    public static string? Problem(JsonElement properties, string bridge)
    {
        try
        {
            var missing = new List<string>();
            if (!properties.TryGetProperty("MachineMatrixString", out var matrix) || !properties.TryGetProperty("MachinePool", out var pool)) return null;
            var layout = MatrixNames(matrix);
            if (layout == null || !layout.Any(n => n.Trim().Equals(bridge.Trim(), StringComparison.OrdinalIgnoreCase)))
            {
                missing.Add("MachineMatrixString (the layout)");
            }
            var poolProblem = PoolProblem(pool, bridge);
            if (poolProblem != null) missing.Add(poolProblem);
            return missing.Count == 0
                ? null
                : $"{bridge} isn't set up in Mouse Without Borders: {string.Join("; ", missing)}. Fix settings.json with PowerToys closed (see the README's \"On the Windows side\").";
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// The layout's four slots, or null when PowerToys would discard it.
    /// It's stored as a list (or wrapped in "value", or one string), and
    /// PowerToys joins it with commas and keeps it only as exactly four
    /// comma-separated names (so a comma in a name breaks it).
    static string[]? MatrixNames(JsonElement m)
    {
        if (m.ValueKind == JsonValueKind.Object && m.TryGetProperty("value", out var v)) m = v;
        var joined = m.ValueKind switch
        {
            JsonValueKind.Array => string.Join(",", m.EnumerateArray().Select(e => e.GetString() ?? "")),
            JsonValueKind.String => m.GetString() ?? "",
            _ => null,
        };
        var slots = joined?.Split(',');
        return slots?.Length == 4 ? slots : null;
    }

    /// What's wrong with `bridge`'s place in the pool ("Name:ID,..."), as
    /// PowerToys loads it: at least four comma-separated slots, only the
    /// first four read, each exactly "name:id" with the name trimmed, and an
    /// ID that isn't a positive number makes switching to it do nothing.
    static string? PoolProblem(JsonElement p, string bridge)
    {
        if (p.ValueKind == JsonValueKind.Object && p.TryGetProperty("value", out var v)) p = v;
        if (p.ValueKind != JsonValueKind.String) return "MachinePool";
        var slots = (p.GetString() ?? "").Split(',');
        if (slots.Length < 4) return "MachinePool (PowerToys needs four comma-separated slots, it ignores this one)";
        foreach (var slot in slots.Take(4))
        {
            var parts = slot.Split(':');
            if (parts.Length != 2 || !parts[0].Trim().Equals(bridge, StringComparison.OrdinalIgnoreCase)) continue;
            return uint.TryParse(parts[1].Trim(), out var id) && id != 0
                ? null
                : $"MachinePool lists {bridge} without a valid ID (it must match the bridge's machine_id)";
        }
        return "MachinePool";
    }
}
