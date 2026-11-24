// Mirror of daemon/protocol/src/companion.rs: MWB's v0.102 encryption
// (PBKDF2-SHA512 key per direction, AES-256-CBC over the whole stream) and
// the frame format on top of it. Keep the two files in step.

using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace MwbBridgeCompanion;

static class Frames
{
    /// Same as the bridge's `PROTOCOL_VERSION`: 2 has `Inactive.DragOver`,
    /// 3 offers folders as relative file names ("folder/sub/a.txt").
    public const uint Version = 3;

    public const uint Hello = 1;
    public const uint Active = 2;
    public const uint Inactive = 3;
    public const uint OfferToWindows = 4;
    public const uint OfferToMac = 5;
    public const uint ClipboardToMac = 6;
    public const uint Cancel = 7;
    public const uint ClipboardToWindows = 8;
    public const uint FileHeader = 10;
    public const uint FileChunk = 11;
    public const uint FileEnd = 12;
    public const uint Done = 13;

    public const int ChunkSize = 1 << 20;
    const ulong MaxFrame = 4 << 20;

    public static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    public static int Padded(int len) => (len + 15) / 16 * 16;
    public static ulong MaxFrameSize => MaxFrame;
}

record HelloMsg(string Role, string Name, string? Op = null, ulong? Id = null, uint Version = 0);
record FileInfoMsg(string Name, ulong Size);
record OfferMsg(ulong Id, List<FileInfoMsg> Files);
/// Version 2+: `DragOver`, a drag from Windows was still over the bridge's
/// machine when control came back; `LetGo`, the button was let go of there
/// since control went there (or the bridge lost its connection meanwhile).
/// `ButtonHeld` is what version 1 sent instead (the bridge's button
/// state); bridges still send it for older companions.
record InactiveMsg(bool DragOver = false, bool? ButtonHeld = null, bool LetGo = false)
{
    /// Whether the drag came back to Windows (still held), read the way the
    /// bridge's version means it. Otherwise it was let go over there.
    public bool DragCameBack(uint version) => version >= 2 ? DragOver || !LetGo : ButtonHeld == true;
}
record CancelMsg(ulong Id);
record FileHeaderMsg(uint Index, string Name, ulong Size);

/// One encrypted connection, split into a locked writer and a reader.
sealed class FrameConnection : IDisposable
{
    const int SaltSize = 16;
    const int Iterations = 100_000;

    readonly NetworkStream _stream;
    readonly ICryptoTransform _encrypt;
    readonly ICryptoTransform _decrypt;
    readonly object _writeLock = new();

    FrameConnection(NetworkStream stream, ICryptoTransform encrypt, ICryptoTransform decrypt)
    {
        _stream = stream;
        _encrypt = encrypt;
        _decrypt = decrypt;
    }

    /// Both ends write their salt, IV and an encrypted priming block, then
    /// read the other's: same as the bridge's `open_encrypted_stream`.
    public static FrameConnection Open(TcpClient client, string securityKey)
    {
        var stream = client.GetStream();
        var (salt, ownKey) = OwnSalt(securityKey);
        var header = new byte[SaltSize + 16];
        salt.CopyTo(header, 0);
        RandomNumberGenerator.Fill(header.AsSpan(SaltSize));
        stream.Write(header);
        var encrypt = Transform(ownKey, header[SaltSize..], encrypting: true);
        var priming = RandomNumberGenerator.GetBytes(16);
        stream.Write(Apply(encrypt, priming));

        var peerHeader = ReadExactly(stream, SaltSize + 16);
        var decrypt = Transform(PeerKey(securityKey, peerHeader[..SaltSize]), peerHeader[SaltSize..], encrypting: false);
        Apply(decrypt, ReadExactly(stream, 16));
        return new FrameConnection(stream, encrypt, decrypt);
    }

    // A derivation takes tens of milliseconds and each connection needs one
    // per direction, so both are cached, as the bridge does: this side keeps
    // one salt per security key for the life of the process (only the IV is
    // new per connection, which is all CBC needs), and the peer's keys are
    // kept by salt (the bridge reuses its salt the same way).
    static readonly object KeysLock = new();
    static readonly Dictionary<string, (byte[] Salt, byte[] Key)> OwnSalts = new();
    static readonly Dictionary<string, byte[]> PeerKeys = new();
    const int PeerKeysKept = 32;

    internal static (byte[] Salt, byte[] Key) OwnSalt(string securityKey)
    {
        lock (KeysLock)
        {
            if (!OwnSalts.TryGetValue(securityKey, out var own))
            {
                var salt = RandomNumberGenerator.GetBytes(SaltSize);
                own = (salt, Derive(securityKey, salt));
                OwnSalts[securityKey] = own;
            }
            return own;
        }
    }

    internal static byte[] PeerKey(string securityKey, byte[] salt)
    {
        var id = securityKey + "\0" + Convert.ToHexString(salt);
        lock (KeysLock)
        {
            if (PeerKeys.TryGetValue(id, out var cached)) return cached;
        }
        // Derived outside the lock, so other connections don't wait on it.
        var key = Derive(securityKey, salt);
        lock (KeysLock)
        {
            if (PeerKeys.Count >= PeerKeysKept) PeerKeys.Clear();
            PeerKeys[id] = key;
        }
        return key;
    }

    internal static byte[] Derive(string securityKey, byte[] salt) =>
        Rfc2898DeriveBytes.Pbkdf2(Encoding.UTF8.GetBytes(securityKey), salt, Iterations, HashAlgorithmName.SHA512, 32);

    static ICryptoTransform Transform(byte[] key, byte[] iv, bool encrypting)
    {
        using var aes = Aes.Create();
        aes.Mode = CipherMode.CBC;
        aes.Padding = PaddingMode.None;
        // TransformBlock keeps the CBC chain between calls.
        return encrypting ? aes.CreateEncryptor(key, iv) : aes.CreateDecryptor(key, iv);
    }

    static byte[] Apply(ICryptoTransform t, byte[] data)
    {
        // TransformBlock rejects an empty input, and empty frames (Active,
        // FileEnd...) have no body.
        if (data.Length == 0) return data;
        var output = new byte[data.Length];
        t.TransformBlock(data, 0, data.Length, output, 0);
        return output;
    }

    static byte[] ReadExactly(Stream s, int count)
    {
        var buf = new byte[count];
        s.ReadExactly(buf);
        return buf;
    }

    public void WriteFrame(uint type, ReadOnlySpan<byte> payload)
    {
        var buf = new byte[16 + Frames.Padded(payload.Length)];
        BitConverter.GetBytes(type).CopyTo(buf, 0);
        BitConverter.GetBytes((ulong)payload.Length).CopyTo(buf, 8);
        payload.CopyTo(buf.AsSpan(16));
        lock (_writeLock)
        {
            _stream.Write(Apply(_encrypt, buf));
        }
    }

    public void WriteJson<T>(uint type, T value) => WriteFrame(type, JsonSerializer.SerializeToUtf8Bytes(value, Frames.Json));

    public (uint Type, byte[] Body) ReadFrame()
    {
        var header = Apply(_decrypt, ReadExactly(_stream, 16));
        var type = BitConverter.ToUInt32(header, 0);
        var len = BitConverter.ToUInt64(header, 8);
        if (len > Frames.MaxFrameSize)
        {
            throw new IOException($"frame of {len} bytes is too big (wrong security key?)");
        }
        var body = Apply(_decrypt, ReadExactly(_stream, Frames.Padded((int)len)));
        return (type, body[..(int)len]);
    }

    public T ReadJson<T>(uint expected)
    {
        var (type, body) = ReadFrame();
        if (type != expected)
        {
            throw new IOException($"expected frame {expected}, got {type}");
        }
        return JsonSerializer.Deserialize<T>(body, Frames.Json) ?? throw new IOException("empty message");
    }

    public static T Parse<T>(byte[] body) => JsonSerializer.Deserialize<T>(body, Frames.Json) ?? throw new IOException("empty message");

    /// Per file a header, its bytes, an end; then Done.
    public void SendFiles(IReadOnlyList<(string Path, string Name)> files)
    {
        var buf = new byte[Frames.ChunkSize];
        for (var i = 0; i < files.Count; i++)
        {
            FileStream? opened = null;
            try { opened = File.OpenRead(files[i].Path); }
            catch (Exception e) { Log.Write($"Couldn't read {files[i].Path}, sending it empty: {e.Message}"); }
            // A file that went away since the offer goes empty, so the rest
            // still arrive at their indexes.
            using var file = opened ?? new FileStream(Path.GetTempFileName(), FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 1, FileOptions.DeleteOnClose);
            WriteJson(Frames.FileHeader, new FileHeaderMsg((uint)i, files[i].Name, (ulong)file.Length));
            int n;
            while ((n = file.Read(buf, 0, buf.Length)) > 0)
            {
                WriteFrame(Frames.FileChunk, buf.AsSpan(0, n));
            }
            WriteFrame(Frames.FileEnd, ReadOnlySpan<byte>.Empty);
        }
        WriteFrame(Frames.Done, ReadOnlySpan<byte>.Empty);
    }

    /// Receives what SendFiles sends; file `index` goes to `dest(header)`.
    public void ReceiveFiles(Func<FileHeaderMsg, string> dest, Action<int> fileDone)
    {
        FileStream? current = null;
        FileHeaderMsg? header = null;
        ulong received = 0;
        while (true)
        {
            var (type, body) = ReadFrame();
            switch (type)
            {
                case Frames.FileHeader:
                    header = Parse<FileHeaderMsg>(body);
                    current = new FileStream(dest(header), FileMode.Create, FileAccess.Write, FileShare.ReadWrite);
                    received = 0;
                    break;
                case Frames.FileChunk:
                    if (current == null) throw new IOException("file data before its header");
                    current.Write(body);
                    received += (ulong)body.Length;
                    break;
                case Frames.FileEnd:
                    if (current == null || header == null) throw new IOException("file end before its header");
                    current.Flush(true);
                    current.Dispose();
                    current = null;
                    if (received != header.Size) throw new IOException($"{header.Name} arrived with {received} of {header.Size} bytes");
                    fileDone((int)header.Index);
                    break;
                case Frames.Done:
                    return;
                default:
                    throw new IOException($"unexpected frame {type} in a file transfer");
            }
        }
    }

    public void Dispose()
    {
        _stream.Dispose();
        _encrypt.Dispose();
        _decrypt.Dispose();
    }
}
