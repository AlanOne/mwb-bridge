namespace MwbBridgeCompanion;

static class Log
{
    static readonly object Lock = new();
    public static string Dir { get; } = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "MwbBridgeCompanion");
    public static string FilePath { get; } = Path.Combine(Dir, "companion.log");

    public static void Write(string message)
    {
        var line = $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} {message}";
        lock (Lock)
        {
            try
            {
                Directory.CreateDirectory(Dir);
                // Start over past 5 MB rather than grow forever.
                if (File.Exists(FilePath) && new FileInfo(FilePath).Length > 5_000_000)
                {
                    File.Move(FilePath, FilePath + ".old", overwrite: true);
                }
                File.AppendAllText(FilePath, line + Environment.NewLine);
            }
            catch
            {
                // Logging must never take the app down.
            }
        }
    }
}
