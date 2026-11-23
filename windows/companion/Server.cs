using System.Net;
using System.Net.Sockets;

namespace MwbBridgeCompanion;

/// Accepts the bridge's connections: one long-lived control connection,
/// plus a short data connection per file transfer.
sealed class Server
{
    readonly Config _config;
    readonly Coordinator _coordinator;
    readonly Action<string?> _status;
    FrameConnection? _control;
    readonly object _controlLock = new();

    public Server(Config config, Coordinator coordinator, Action<string?> status)
    {
        _config = config;
        _coordinator = coordinator;
        _status = status;
        coordinator.SendToBridge = Send;
    }

    public void Start()
    {
        var listener = new TcpListener(IPAddress.IPv6Any, _config.Port);
        listener.Server.DualMode = true;
        listener.Start();
        Log.Write($"Listening on port {_config.Port} for the bridge.");
        new Thread(() =>
        {
            while (true)
            {
                var client = listener.AcceptTcpClient();
                new Thread(() => Serve(client)) { IsBackground = true }.Start();
            }
        }) { IsBackground = true }.Start();
    }

    bool Send(uint type, object message)
    {
        lock (_controlLock)
        {
            if (_control == null) return false;
            try
            {
                _control.WriteJson(type, message);
                return true;
            }
            catch (Exception e)
            {
                Log.Write($"Sending to the bridge failed: {e.Message}");
                return false;
            }
        }
    }

    void Serve(TcpClient client)
    {
        var peer = client.Client.RemoteEndPoint;
        FrameConnection? conn = null;
        try
        {
            client.NoDelay = true;
            conn = FrameConnection.Open(client, _config.SecurityKey ?? throw new IOException("no security key"));
            var hello = conn.ReadJson<HelloMsg>(Frames.Hello);
            if (hello.Role == "control")
            {
                RunControl(conn, hello, peer);
            }
            else if (hello.Role == "data" && hello.Id is ulong id)
            {
                RunData(conn, hello.Op, id);
            }
        }
        catch (Exception e)
        {
            Log.Write($"Connection from {peer} ended: {e.Message}");
        }
        finally
        {
            conn?.Dispose();
            client.Dispose();
        }
    }

    void RunControl(FrameConnection conn, HelloMsg hello, EndPoint? peer)
    {
        lock (_controlLock)
        {
            _control?.Dispose(); // a reconnect replaces the old connection
            _control = conn;
        }
        Log.Write($"Bridge {hello.Name} connected from {peer}.");
        _status(hello.Name);
        try
        {
            while (true)
            {
                var (type, body) = conn.ReadFrame();
                switch (type)
                {
                    case Frames.Active:
                        _coordinator.Post(_coordinator.OnActive);
                        break;
                    case Frames.Inactive:
                        var inactive = FrameConnection.Parse<InactiveMsg>(body);
                        _coordinator.Post(() => _coordinator.OnInactive(inactive.ButtonHeld));
                        break;
                    case Frames.OfferToWindows:
                        // Registered here, not on the UI thread, so the
                        // files' data connection can't arrive before it.
                        var drop = _coordinator.Register(FrameConnection.Parse<OfferMsg>(body));
                        _coordinator.Post(() => _coordinator.OnOfferToWindows(drop));
                        break;
                    case Frames.ClipboardToWindows:
                        _coordinator.RegisterCopy(FrameConnection.Parse<OfferMsg>(body));
                        break;
                    case Frames.Cancel:
                        var cancel = FrameConnection.Parse<CancelMsg>(body);
                        _coordinator.Post(() => _coordinator.OnCancel(cancel.Id));
                        break;
                    default:
                        Log.Write($"Ignoring control frame {type}.");
                        break;
                }
            }
        }
        finally
        {
            lock (_controlLock)
            {
                if (_control == conn) _control = null;
            }
            _status(null);
            _coordinator.Post(_coordinator.OnBridgeDisconnected);
            Log.Write($"Bridge {hello.Name} disconnected.");
        }
    }

    void RunData(FrameConnection conn, string? op, ulong id)
    {
        if (op == "get")
        {
            var files = _coordinator.FilesToMac(id) ?? throw new IOException($"no drag {id} to send");
            conn.SendFiles(files);
            Log.Write($"[to-mac] Sent drag {id}'s {files.Length} file(s).");
        }
        else if (op == "put")
        {
            // The offer comes over the control connection; give it a moment.
            IncomingDrop? drop = null;
            for (var i = 0; i < 40 && (drop = _coordinator.FromMac(id)) == null; i++) Thread.Sleep(50);
            if (drop == null) throw new IOException($"no drag {id} to receive");
            try
            {
                conn.ReceiveFiles(h => drop.Paths[h.Index], drop.FileDone);
            }
            catch
            {
                drop.Failed = true;
                throw;
            }
        }
    }
}
