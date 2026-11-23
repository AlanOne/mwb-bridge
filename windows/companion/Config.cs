using System.Text.Json;

namespace MwbBridgeCompanion;

/// `%APPDATA%\MwbBridgeCompanion\config.json`, all optional:
/// `{"security_key": "...", "port": 15102}`. Without a `security_key` the
/// one PowerToys' Mouse Without Borders uses is read from its own settings,
/// on every connection, so a key changed there is picked up by itself.
sealed class Config
{
    public static string FilePath { get; } = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MwbBridgeCompanion", "config.json");

    static string MwbSettingsPath { get; } = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "Microsoft", "PowerToys", "MouseWithoutBorders", "settings.json");

    readonly string? _explicitKey;
    public int Port { get; }

    Config(string? explicitKey, int port)
    {
        _explicitKey = explicitKey;
        Port = port;
    }

    /// The key for a new connection, or null when there's none to be found.
    public string? SecurityKey => !string.IsNullOrEmpty(_explicitKey) ? _explicitKey : MwbProperty("SecurityKey")?.GetString();

    public static Config Load()
    {
        string? key = null;
        var port = 15102;
        if (File.Exists(FilePath))
        {
            try
            {
                var json = JsonDocument.Parse(File.ReadAllText(FilePath)).RootElement;
                if (json.TryGetProperty("security_key", out var k)) key = k.GetString();
                if (json.TryGetProperty("port", out var p)) port = p.GetInt32();
            }
            catch (Exception e)
            {
                Log.Write($"Couldn't read {FilePath}, using defaults: {e.Message}");
            }
        }
        return new Config(key, port);
    }

    /// Whether PowerToys' own file drag-and-drop and file copy ("Transfer
    /// file") is on. It catches the same drags this app does, so the two fight.
    public static bool MwbTransferFileOn() => MwbProperty("TransferFile")?.GetBoolean() == true;

    /// A `properties.<name>.value` from Mouse Without Borders' settings.
    static JsonElement? MwbProperty(string name)
    {
        try
        {
            var props = JsonDocument.Parse(File.ReadAllText(MwbSettingsPath)).RootElement.GetProperty("properties");
            return props.GetProperty(name).GetProperty("value").Clone();
        }
        catch
        {
            return null;
        }
    }
}
