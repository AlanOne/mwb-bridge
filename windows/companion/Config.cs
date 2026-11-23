using System.Text.Json;

namespace MwbBridgeCompanion;

sealed record Config(string SecurityKey, int Port)
{
    public static string FilePath { get; } = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MwbBridgeCompanion", "config.json");

    /// `{"security_key": "...", "port": 15102}`; the key is the same one
    /// PowerToys' Mouse Without Borders and the bridge use.
    public static Config? Load()
    {
        try
        {
            var json = JsonDocument.Parse(File.ReadAllText(FilePath)).RootElement;
            var key = json.GetProperty("security_key").GetString();
            var port = json.TryGetProperty("port", out var p) ? p.GetInt32() : 15102;
            return string.IsNullOrEmpty(key) ? null : new Config(key, port);
        }
        catch (Exception e)
        {
            Log.Write($"Couldn't read {FilePath}: {e.Message}");
            return null;
        }
    }

    /// Whether PowerToys' own file drag-and-drop ("Transfer file") is on.
    /// It catches the same drags this app does, so the two fight.
    public static bool MwbTransferFileOn()
    {
        try
        {
            var path = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "Microsoft", "PowerToys", "MouseWithoutBorders", "settings.json");
            var props = JsonDocument.Parse(File.ReadAllText(path)).RootElement.GetProperty("properties");
            return props.GetProperty("TransferFile").GetProperty("value").GetBoolean();
        }
        catch
        {
            return false;
        }
    }
}
